// starfane web console — workspace view (host-grouped sidebar + rename, reading cursor history,
// chunked full-history terminal + Nerd Font glyph toggle, prompt outline rail)
import {
  AGENTS,
  AGENT_DISPLAY,
  LIVE_TAIL_LINES,
  NON_TMUX_GROUP,
  NON_TMUX_OPEN_KEY,
  OUTLINE_COLLAPSED_KEY,
  PASTE_CHAR_LIMIT,
  PASTE_LINE_LIMIT,
  PREVIEW_PAGE_STEP,
  PREVIEW_STEPS,
  STATE_PRIO,
  TRANSCRIPT_LIMIT,
  TRANSCRIPT_PAGE_STEP,
} from './constants.js';
import {
  $,
  agentTag,
  btn,
  buildTelemetryStrip,
  clock,
  ctxBar,
  ctxPctOf,
  ctxTone,
  deliveryText,
  emptyState,
  errText,
  errorBanner,
  field,
  fmtBytes,
  fmtMs,
  h,
  isFin,
  skeleton,
  span,
  statusInd,
  toast,
  tpsOf,
  ttftOf,
  ttftTone,
} from './dom.js';
import {
  act,
  api,
  loadAll,
  loadAllTranscript,
  loadEarlierPreview,
  loadEarlierTranscript,
  loadPreview,
  loadSessionOutput,
  loadSlashCommands,
  loadTranscript,
  prefetchSessionOutput,
  probeHostApi,
  pauseHostRetryApi,
  qs,
  resumeSessionApi,
  sessionNeedsLiveTail,
  suspendSessionApi,
  syncStateSilent,
} from './api.js';
import {
  applySlashItem,
  attachFoldedPaste,
  autoGrow,
  closeSlashMenu,
  quoteReplyToComposer,
  renderAttachBar,
  saveComposerSelection,
  syncSlashActive,
  updateSlashFromInput,
  uploadImageFiles,
  workspacePipelineComposer,
} from './composer.js';
import { renderMarkdown } from './markdown.js';
import { L, getLang } from './i18n.js';
import { canCloseSessionWindow, canResumeSession, canSuspendSession, openCloseSessionModal, windowGroupKeyOf } from './overlays.js';
import {
  APP,
  OPTIMISTIC_EXPIRE_MS,
  S,
  UI,
  addOptimisticTurn,
  base,
  currentSession,
  draft,
  enc,
  fmtRtt,
  formatRetryCountdown,
  groupDisplayName,
  hasOptimisticTurn,
  hashMessage,
  hashStr,
  hostDisplayName,
  hostOf,
  hostStatusLabel,
  isRemoteHost,
  modelEffortLabel,
  muxKindOf,
  paneDisplayId,
  pickDefaultSession,
  pvKeyOf,
  pvModeOf,
  reconcileOptimisticTurns,
  removeOptimisticTurn,
  rootSessions,
  rttTone,
  saveCollapsedGroups,
  saveCollapsedHosts,
  sessionLabel,
  stateText,
  subagentDisplayName,
  subagentsOf,
  tmuxGroupOf,
  updateOptimisticTurnStatus,
  upsertSession,
} from './store.js';
import {
  cycleGlyphMode,
  detectNerdFontSupport,
  effectiveGlyphMode,
  formatTerminalText,
  parseTerminalTurns,
  renderChunkedTerminal,
} from './terminal.js';
import { isInteractivePromptSession, promptPreviewSourceFor, renderInteractivePromptBar } from './views.js';

export { renderInteractivePromptBar };

const TIMELINE_CHUNK_SIZE = 20;
const HOVER_PREFETCH_MS = 120;
const SIDEBAR_WIDTH_KEY = 'starfane_sidebar_width';
const SIDEBAR_DEFAULT_W = 300;
const SIDEBAR_MIN_W = 220;
const SIDEBAR_MAX_W = 560;
const composerDeliveryIssues = new Map();
const SVG_NS = 'http://www.w3.org/2000/svg';

export function sidebarMaxWidth() {
  const vw = (typeof window !== 'undefined' && window.innerWidth) || 1440;
  return Math.max(SIDEBAR_DEFAULT_W, Math.min(SIDEBAR_MAX_W, Math.floor(vw * 0.48)));
}

export function clampSidebarWidth(w) {
  const maxW = sidebarMaxWidth();
  return Math.round(Math.min(maxW, Math.max(SIDEBAR_MIN_W, Number.isFinite(w) ? w : SIDEBAR_DEFAULT_W)));
}

let hoverPrefetchTimer = 0;
let hoverPrefetchSid = null;

function cancelHoverPrefetch(sid = null) {
  if (sid && hoverPrefetchSid !== sid) return;
  if (hoverPrefetchTimer) {
    clearTimeout(hoverPrefetchTimer);
    hoverPrefetchTimer = 0;
  }
  hoverPrefetchSid = null;
}

function scheduleHoverPrefetch(s, e) {
  if (e && e.pointerType === 'touch') return;
  if (!s || !s.session_id || s.state === 'ended') return;
  if (s.session_id === currentSession()?.session_id) return;
  const hid = hostOf(s);
  if (hid !== 'local' && S.hosts.get(hid)?.status === 'offline') return;
  cancelHoverPrefetch();
  hoverPrefetchSid = s.session_id;
  hoverPrefetchTimer = setTimeout(() => {
    hoverPrefetchTimer = 0;
    const targetSid = hoverPrefetchSid;
    hoverPrefetchSid = null;
    if (!targetSid) return;
    const latest = S.sessions.get(targetSid);
    if (latest) prefetchSessionOutput(latest);
  }, HOVER_PREFETCH_MS);
}

function prefetchTopMobileSessions(list) {
  const curId = currentSession()?.session_id;
  let picked = 0;
  for (let i = 0; i < list.length && picked < 2; i++) {
    const s = list[i];
    if (!s || s.session_id === curId || s.state === 'ended') continue;
    const hid = hostOf(s);
    if (hid !== 'local' && S.hosts.get(hid)?.status === 'offline') continue;
    picked++;
    prefetchSessionOutput(s);
  }
}

function wsSvg(attrs, kids = []) {
  const el = document.createElementNS(SVG_NS, 'svg');
  for (const [k, v] of Object.entries(attrs)) {
    if (v != null) el.setAttribute(k, String(v));
  }
  for (const [tag, pattrs] of kids) {
    const child = document.createElementNS(SVG_NS, tag);
    for (const [k, v] of Object.entries(pattrs)) {
      if (v != null) child.setAttribute(k, String(v));
    }
    el.appendChild(child);
  }
  return el;
}

function closeCrossIcon() {
  return wsSvg({
    class: 'ws-close-ico',
    viewBox: '0 0 16 16',
    fill: 'none',
    stroke: 'currentColor',
    'stroke-width': '2.1',
    'stroke-linecap': 'round',
    'aria-hidden': 'true',
  }, [
    ['path', { d: 'M4 4L12 12' }],
    ['path', { d: 'M12 4L4 12' }],
  ]);
}

function menuDrawerIcon() {
  return wsSvg({
    class: 'ws-head-ico',
    viewBox: '0 0 20 20',
    width: '18',
    height: '18',
    fill: 'none',
    stroke: 'currentColor',
    'stroke-width': '1.85',
    'stroke-linecap': 'round',
    'aria-hidden': 'true',
  }, [
    ['line', { x1: 3, y1: 5.5, x2: 17, y2: 5.5 }],
    ['line', { x1: 3, y1: 10, x2: 17, y2: 10 }],
    ['line', { x1: 3, y1: 14.5, x2: 13, y2: 14.5 }],
  ]);
}

function outlineListIcon() {
  return wsSvg({
    class: 'ws-head-ico',
    viewBox: '0 0 20 20',
    width: '15',
    height: '15',
    fill: 'none',
    stroke: 'currentColor',
    'stroke-width': '1.85',
    'stroke-linecap': 'round',
    'aria-hidden': 'true',
  }, [
    ['line', { x1: 7, y1: 5.5, x2: 17, y2: 5.5 }],
    ['line', { x1: 7, y1: 10, x2: 17, y2: 10 }],
    ['line', { x1: 7, y1: 14.5, x2: 14.5, y2: 14.5 }],
    ['circle', { cx: 3.75, cy: 5.5, r: 0.9, fill: 'currentColor', stroke: 'none' }],
    ['circle', { cx: 3.75, cy: 10, r: 0.9, fill: 'currentColor', stroke: 'none' }],
    ['circle', { cx: 3.75, cy: 14.5, r: 0.9, fill: 'currentColor', stroke: 'none' }],
  ]);
}

function modeSwitchIcon(toRaw) {
  if (toRaw) {
    return wsSvg({
      class: 'ws-head-ico',
      viewBox: '0 0 20 20',
      width: '14',
      height: '14',
      fill: 'none',
      stroke: 'currentColor',
      'stroke-width': '1.85',
      'stroke-linecap': 'round',
      'stroke-linejoin': 'round',
      'aria-hidden': 'true',
    }, [
      ['rect', { x: 2.5, y: 3.5, width: 15, height: 13, rx: 2.5 }],
      ['path', { d: 'M6.5 8L9.2 10.2L6.5 12.4' }],
      ['line', { x1: 10.8, y1: 12.4, x2: 13.8, y2: 12.4 }],
    ]);
  }
  return wsSvg({
    class: 'ws-head-ico',
    viewBox: '0 0 20 20',
    width: '14',
    height: '14',
    fill: 'none',
    stroke: 'currentColor',
    'stroke-width': '1.85',
    'stroke-linecap': 'round',
    'stroke-linejoin': 'round',
    'aria-hidden': 'true',
  }, [
    ['rect', { x: 3.5, y: 3, width: 13, height: 14, rx: 2 }],
    ['line', { x1: 6.5, y1: 7.2, x2: 13.5, y2: 7.2 }],
    ['line', { x1: 6.5, y1: 10.2, x2: 13.5, y2: 10.2 }],
    ['line', { x1: 6.5, y1: 13.2, x2: 11, y2: 13.2 }],
  ]);
}

function buildMobCtxEl(s) {
  const pctVal = ctxPctOf(s?.metrics);
  if (pctVal == null) return h('span', { class: 'ws-mob-ctx ws-hidden', 'data-mob-tel-sid': s?.session_id || '' });
  const tone = ctxTone(pctVal);
  const toneCls = tone === 'high' ? ' ws-tel-high' : tone === 'warn' ? ' ws-tel-warn' : '';
  return h('span', {
    class: `ws-mob-ctx${toneCls}`,
    'data-mob-tel-sid': s?.session_id || '',
  }, `CTX ${Math.round(pctVal)}%`);
}

function describeDeliveryIssue(r) {
  if (!r || typeof r !== 'object') {
    return L('服务端未返回投递结果', 'Server did not return a delivery report');
  }
  const rawDetail = String(r.gate?.detail || '').trim();
  const detailLow = rawDetail.toLowerCase();
  const failedChecks = Array.isArray(r.gate?.checks)
    ? r.gate.checks.filter((pair) => Array.isArray(pair) && pair[1] === false).map((pair) => pair[0])
    : [];
  const ack = String(r.ack || '');

  if (ack === 'stuck_in_prompt' || detailLow.includes('stuck in terminal input box')) {
    return L(
      '文本可能仍停留在终端输入框未提交，请点击「查看终端」核对或点击「重试发送」',
      'Message may still be sitting unsubmitted in the terminal input box — inspect terminal or click Retry',
    );
  }
  if (ack === 'copy_mode_race' || detailLow.includes('entered copy-mode during injection')) {
    return L(
      '发送瞬间终端进入了 tmux 复制/滚动模式，点击「重试发送」将自动退出并重新注入',
      'Pane entered tmux copy-mode during injection — click Retry to auto-exit copy-mode and resend',
    );
  }
  if (detailLow.includes('copy-mode') || failedChecks.includes('not_in_mode')) {
    return L(
      '终端 pane 正处于 tmux 交互模式/复制模式，无法直接写入；请退出该模式后重试',
      'Terminal pane is in a tmux interactive/copy mode and cannot accept input; exit that mode and retry',
    );
  }
  if (detailLow.includes('synchronize-panes') || failedChecks.includes('not_synchronized')) {
    return L(
      '该 tmux 窗口已开启 synchronize-panes（同步输入），为避免误发到其他 pane 已拦截',
      'This tmux window has synchronize-panes enabled — blocked to avoid broadcasting to other panes',
    );
  }
  if (
    detailLow.includes('pane is dead')
    || detailLow.includes('exited during injection')
    || detailLow.includes('disappeared during injection')
    || detailLow.includes('not found')
    || failedChecks.includes('pane_alive')
  ) {
    return L(
      `目标终端 pane 已退出或不可用${rawDetail ? `（${rawDetail}）` : ''}`,
      `Target terminal pane has exited or is unavailable${rawDetail ? ` (${rawDetail})` : ''}`,
    );
  }
  if (r.status === 'queued') {
    return L(
      `消息仅暂存到收件箱，未能直接注入终端${rawDetail ? `（${rawDetail}）` : ''}`,
      `Message was queued in inbox only and not injected into the terminal${rawDetail ? ` (${rawDetail})` : ''}`,
    );
  }
  if (rawDetail) return rawDetail;
  return deliveryText(r);
}

export function updateOutputClock(sessionId) {
  if (UI.view !== 'workspace' || currentSession()?.session_id !== sessionId) return;
  const el = $('.term-updated');
  if (el) el.textContent = L(`更新于 ${clock(Date.now())}`, `Updated ${clock(Date.now())}`);
}

function telemetrySigOf(s) {
  const m = s?.metrics;
  if (!m) return `${s?.session_id || ''}:none:${getLang()}`;
  return `${s.session_id}:${m.source || ''}:${m.context_used_pct ?? ''}:${m.context_used_tokens ?? ''}:${m.context_window_tokens ?? ''}:${m.tps ?? ''}:${m.ttft_ms ?? ''}:${m.avg_ttft_ms ?? ''}:${m.recent_turns?.length || 0}:${m.recent_turns?.at(-1)?.ts_ms || 0}:${m.total_input_tokens ?? ''}:${m.total_output_tokens ?? ''}:${m.total_cache_read_tokens ?? ''}:${m.last_output_tokens ?? ''}:${m.last_duration_ms ?? ''}:${getLang()}`;
}

export function patchTelemetryStrip(sessionId) {
  if (UI.view !== 'workspace' || currentSession()?.session_id !== sessionId) return;
  const s = S.sessions.get(sessionId);
  const W = UI.wsShell;
  const el = W?.headEl?.querySelector('.ws-telemetry') || $('.ws-telemetry');
  if (el && s && el.dataset.telSid === sessionId) {
    const nextTelSig = telemetrySigOf(s);
    if (W && W.telSig === nextTelSig) return;
    el.replaceWith(buildTelemetryStrip(s));
    const mobCtx = W?.headEl?.querySelector('.ws-mob-ctx[data-mob-tel-sid]');
    if (mobCtx && mobCtx.dataset.mobTelSid === sessionId) mobCtx.replaceWith(buildMobCtxEl(s));
    if (W) W.telSig = nextTelSig;
  }
}

function setOutlineActive(rail, activeIdx) {
  if (!rail) return;
  const want = activeIdx != null ? String(activeIdx) : '';
  if (rail._activeIdx === want) return;
  const prevEl = rail._activeIdx && rail._itemByIdx ? rail._itemByIdx.get(rail._activeIdx) : null;
  if (prevEl) prevEl.classList.remove('active');
  const nextEl = want && rail._itemByIdx ? rail._itemByIdx.get(want) : null;
  if (nextEl) nextEl.classList.add('active');
  rail._activeIdx = want;
}

/**
 * Chunk-first outline scroll spy: checks outer `.timeline-chunk` bounding boxes via binary search
 * so off-screen `content-visibility: auto` chunks are NEVER de-virtualized on scroll, and updates
 * active classes in O(1) via `rail._itemByIdx`.
 */
export function syncOutlineSpy(vp) {
  if (!vp || Date.now() < UI.jumpLockUntil) return;
  const rail = UI.wsShell?.rail;
  if (!rail || !rail.isConnected || !rail._itemByIdx?.size) return;
  const vpRect = vp.getBoundingClientRect();
  const anchorY = vpRect.top + Math.min(160, vpRect.height * 0.35);
  const tv = vp._tv;
  if (tv?.chunkOrder?.length) {
    const order = tv.chunkOrder;
    let lo = 0;
    let hi = order.length - 1;
    let hit = -1;
    while (lo <= hi) {
      const mid = (lo + hi) >> 1;
      const entry = tv.chunkMap.get(order[mid]);
      if (!entry) break;
      const top = entry.el.getBoundingClientRect().top;
      if (top <= anchorY) {
        hit = mid;
        lo = mid + 1;
      } else {
        hi = mid - 1;
      }
    }
    let activeIdx = null;
    if (hit >= 0) {
      const hitEntry = tv.chunkMap.get(order[hit]);
      if (hitEntry?.userTurns?.length) {
        const rect = hitEntry.el.getBoundingClientRect();
        if (rect.bottom <= anchorY) {
          activeIdx = hitEntry.userTurns[hitEntry.userTurns.length - 1].idx;
        } else {
          for (let i = 0; i < hitEntry.userTurns.length; i++) {
            const ut = hitEntry.userTurns[i];
            if (ut.el.getBoundingClientRect().top <= anchorY) activeIdx = ut.idx;
            else break;
          }
        }
      }
      for (let c = hit - 1; activeIdx == null && c >= 0; c--) {
        const prevEntry = tv.chunkMap.get(order[c]);
        if (prevEntry?.userTurns?.length) {
          activeIdx = prevEntry.userTurns[prevEntry.userTurns.length - 1].idx;
        }
      }
    }
    if (activeIdx == null) {
      for (let c = 0; c < order.length; c++) {
        const entry = tv.chunkMap.get(order[c]);
        if (entry?.userTurns?.length) {
          activeIdx = entry.userTurns[0].idx;
          break;
        }
      }
    }
    if (activeIdx != null) setOutlineActive(rail, activeIdx);
    return;
  }
  const items = [...rail.querySelectorAll('.outline-item[data-turn-idx]')];
  if (!items.length) return;
  let activeIdx = items[0].dataset.turnIdx;
  for (const it of items) {
    const el = document.getElementById(`turn-u-${it.dataset.turnIdx}`);
    if (!el) continue;
    if (el.getBoundingClientRect().top <= anchorY) activeIdx = it.dataset.turnIdx;
  }
  setOutlineActive(rail, activeIdx);
}

export function syncScrollLatestBtn(vp) {
  if (!vp) return;
  const b = UI.wsShell?.latestBtn || $('.scroll-latest-btn');
  if (b) {
    const dist = vp.scrollHeight - vp.scrollTop - vp.clientHeight;
    b.classList.toggle('show', dist > 120);
  }
  syncOutlineSpy(vp);
}

const promptCache = new Map();

function collectUserPromptsCached(s) {
  if (!s || s.agent === 'shell') return { prompts: [], sig: 'empty' };
  const tr = S.transcripts.get(s.session_id);
  const useTr = tr?.state === 'ok' && tr.source === 'transcript' && tr.messages.length > 0;
  const pv = !useTr && s.pane ? S.previews.get(pvKeyOf(s.pane)) : null;
  const cacheKey = useTr
    ? `tr:${s.session_id}:${tr.hash}:${getLang()}`
    : `pv:${s.session_id}:${pv?.hash || ''}:${getLang()}`;
  const hit = promptCache.get(cacheKey);
  if (hit) return hit;

  let msgs = [];
  if (useTr) msgs = tr.messages;
  else if (pv?.text?.trim()) msgs = parseTerminalTurns(pv.text, s.agent);

  const out = [];
  let uIdx = 0;
  const sigParts = [];
  for (let i = 0; i < msgs.length; i++) {
    const m = msgs[i];
    if (m.role !== 'user') continue;
    const preview = String(m.text || '').replace(/\s+/g, ' ').trim();
    if (
      preview.startsWith('[后台任务')
      || preview.startsWith('[Swarm 通知]')
      || /^\[来自 [^\]]+ 消息\]/.test(preview)
      || preview.startsWith('Task id "')
    ) {
      continue;
    }
    uIdx++;
    const txt = preview || L('（空消息）', '(empty message)');
    out.push({ idx: uIdx, text: txt, ts_ms: m.ts_ms || null });
    sigParts.push(`${uIdx}:${m.ts_ms || 0}:${hashStr(txt)}`);
  }
  const entry = { prompts: out, sig: `${out.length}:${hashStr(sigParts.join('|'))}` };
  if (promptCache.size >= 16) {
    const oldest = promptCache.keys().next().value;
    if (oldest !== undefined) promptCache.delete(oldest);
  }
  promptCache.set(cacheKey, entry);
  return entry;
}

export function collectUserPrompts(s) {
  return collectUserPromptsCached(s).prompts;
}

function scrollViewportToBottom(vp) {
  if (!vp) return;
  vp.scrollTop = vp.scrollHeight;
  requestAnimationFrame(() => {
    if (!vp.isConnected) return;
    vp.scrollTop = vp.scrollHeight;
    syncScrollLatestBtn(vp);
  });
}

/**
 * Extract only the trailing `maxLines` lines from raw terminal capture before running
 * ANSI-strip and Nerd Font glyph normalization, avoiding O(full scrollback) work on 3s ticks.
 */
function extractLiveTail(rawText, maxLines = LIVE_TAIL_LINES) {
  if (!rawText) return '';
  const s = String(rawText);
  let end = s.length;
  while (end > 0 && (s.charCodeAt(end - 1) === 10 || s.charCodeAt(end - 1) === 13 || s.charCodeAt(end - 1) === 32)) {
    end--;
  }
  if (end === 0) return '';
  let idx = end;
  let nlCount = 0;
  const scanLines = maxLines + 8;
  while (idx > 0 && nlCount < scanLines) {
    const found = s.lastIndexOf('\n', idx - 1);
    if (found < 0) { idx = 0; break; }
    nlCount++;
    idx = found;
  }
  const slice = s.slice(idx > 0 ? idx + 1 : 0, end);
  const formatted = formatTerminalText(slice).trim();
  if (!formatted) return '';
  const lines = formatted.split('\n');
  return lines.length > maxLines ? lines.slice(-maxLines).join('\n') : formatted;
}

function syncOutlineRail(W, s, effectiveMode) {
  if (!W || !s) return;
  const isShell = s.agent === 'shell';
  const cached = isShell ? { prompts: [], sig: 'empty' } : collectUserPromptsCached(s);
  const prompts = cached.prompts;
  const showMobToc = effectiveMode === 'reading' && prompts.length > 0;
  const mobBtn = W.mobOutlineBtn || W.headEl?.querySelector('.ws-outline-mob-btn');
  if (mobBtn) {
    mobBtn.classList.toggle('ws-hidden', !showMobToc);
    if (showMobToc) {
      const cntStr = String(prompts.length);
      if (mobBtn.dataset.cnt !== cntStr) {
        mobBtn.dataset.cnt = cntStr;
        const cntEl = mobBtn.querySelector('.ws-outline-cnt');
        if (cntEl) cntEl.textContent = cntStr;
      }
      const lbl = L(`打开提问目录（${prompts.length} 条）`, `Open prompt outline (${prompts.length})`);
      mobBtn.setAttribute('aria-label', lbl);
      mobBtn.title = lbl;
    }
  }
  const railSig = `${s.session_id}|${effectiveMode}|${UI.outlineCollapsed ? 1 : 0}|${getLang()}|${cached.sig}`;
  if (W.railSig !== railSig || (!UI.outlineCollapsed && prompts.length > 0 && effectiveMode === 'reading' && !W.rail?.isConnected)) {
    W.railSig = railSig;
    const showReopen = effectiveMode === 'reading' && prompts.length > 0 && UI.outlineCollapsed;
    W.reopenBtn.classList.toggle('ws-hidden', !showReopen);
    if (showReopen) {
      W.reopenBtn.dataset.cnt = String(prompts.length);
      W.reopenBtn.textContent = L(`« 目录 (${prompts.length})`, `« Outline (${prompts.length})`);
    }
    if (W.rail) { W.rail.remove(); W.rail = null; }
    const nextRail = outlineRailPane(s, prompts);
    if (nextRail) { W.rail = nextRail; W.bodySplit.appendChild(nextRail); }
  }
}

function syncInteractivePromptBar(W, s) {
  if (!W?.promptBarSlot) return;
  if (!isInteractivePromptSession(s)) {
    if (W.promptBarSig || !W.promptBarSlot.classList.contains('ws-hidden')) {
      W.promptBarSig = '';
      W.promptBarSlot.classList.add('ws-hidden');
      W.promptBarSlot.replaceChildren();
    }
    return;
  }
  const sid = s.session_id;
  const pvSrc = promptPreviewSourceFor(s);
  const busySig = [
    UI.busy.has(`key:${sid}`) ? 1 : 0,
    UI.busy.has(`key:${sid}:Up`) ? 1 : 0,
    UI.busy.has(`key:${sid}:Down`) ? 1 : 0,
    UI.busy.has(`key:${sid}:Enter`) ? 1 : 0,
    UI.busy.has(`key:${sid}:Escape`) ? 1 : 0,
  ].join('');
  const sig = `${sid}|${s.state}|${s.pane || ''}|${hashStr(s.last_error || '')}|${pvSrc.hash}|${busySig}|${UI.termGlyphMode}:${effectiveGlyphMode()}|${getLang()}`;
  if (W.promptBarSig === sig && !W.promptBarSlot.classList.contains('ws-hidden')) return;
  W.promptBarSig = sig;
  const node = renderInteractivePromptBar(s, { variant: 'workspace' });
  if (node) {
    W.promptBarSlot.classList.remove('ws-hidden');
    W.promptBarSlot.replaceChildren(node);
  } else {
    W.promptBarSlot.classList.add('ws-hidden');
    W.promptBarSlot.replaceChildren();
  }
}

export function notifyOutputUpdated(sessionId) {
  if (UI.view !== 'workspace' || currentSession()?.session_id !== sessionId) { APP.schedulePassive(); return; }
  const W = UI.wsShell;
  const vp = W?.vp || $('.term-viewport');
  const s = currentSession();
  const effMode = s?.agent === 'shell' ? 'raw' : UI.outputMode;
  if (!vp || !s || vp.dataset.sid !== s.session_id || vp.dataset.mode !== effMode) {
    APP.schedule();
    return;
  }
  syncOutputViewport(vp, s);
  updateOutputClock(sessionId);
  if (W) {
    syncOutlineRail(W, s, effMode);
    syncInteractivePromptBar(W, s);
  }
}

export function ackSessionError(s) {
  if (!s) return;
  return act(`ack:${s.session_id}`, async () => {
    const updated = await api('POST', `/api/sessions/${enc(s.session_id)}/ack`, {});
    if (updated?.session_id) upsertSession(updated);
    syncStateSilent();
    return updated;
  }, L('已清除异常状态', 'Error state cleared'));
}

function sessionAlertBanner(s) {
  if (!s || (s.state !== 'error' && !s.last_error)) return null;
  if (isInteractivePromptSession(s)) return null;
  const detail = s.last_error || L('Agent 上报了错误状态，请查看下方终端实时行或事件记录。', 'Agent reported an error state — check the live terminal tail or recent events below.');
  return h('div', { class: 'callout err' },
    h('div', { class: 'stack-xs grow' },
      h('strong', null, L(`会话${stateText(s.state)}`, `Session: ${stateText(s.state)}`)),
      h('span', null, detail)),
    btn(L('清除异常状态', 'Clear error state'), { cls: 'sm danger', busyKey: `ack:${s.session_id}`, onclick: () => ackSessionError(s) }));
}

function buildRttPill(rttMs, syncMs = null) {
  if (typeof rttMs !== 'number' || !Number.isFinite(rttMs) || rttMs < 0) return null;
  const label = fmtRtt(rttMs);
  if (!label) return null;
  const hasSync = typeof syncMs === 'number' && Number.isFinite(syncMs) && syncMs >= 0;
  const title = hasSync
    ? L(`网络延迟 ${Math.round(rttMs)}ms · 同步耗时 ${Math.round(syncMs)}ms`, `Network RTT ${Math.round(rttMs)}ms · Sync ${Math.round(syncMs)}ms`)
    : L(`往返延迟 ${label}`, `Round-trip latency ${label}`);
  return h('span', {
    class: `tag mono rtt-pill rtt-${rttTone(rttMs)}`,
    title,
  }, label);
}

function buildStaleBanner(s, staleErr, onRetry) {
  if (!staleErr) return null;
  const hid = hostOf(s);
  const remote = isRemoteHost(hid);
  const hRec = remote ? S.hosts.get(hid) : null;
  const hStatus = hRec?.status || 'online';
  const showReconnect = remote && (hStatus === 'degraded' || hStatus === 'offline');
  return h('div', { class: 'stale-banner', role: 'status' },
    h('span', { class: 'stale-banner-msg' },
      h('i', { class: `dot dot-${hStatus === 'offline' ? 'down' : 'degraded'}` }),
      h('span', null, L(
        `同步暂时受阻，正显示缓存内容：${staleErr}`,
        `Sync interrupted — showing cached output: ${staleErr}`,
      ))),
    h('span', { class: 'stale-banner-acts' },
      onRetry ? btn(L('重试', 'Retry'), { cls: 'xs', onclick: onRetry }) : null,
      showReconnect ? btn(L('重连主机', 'Reconnect'), {
        cls: 'xs',
        busyKey: `probe-host:${hid}`,
        onclick: () => probeHostApi(hid),
      }) : null));
}

function buildGlyphTogglePill() {
  const native = detectNerdFontSupport();
  const eff = effectiveGlyphMode();
  const label = native && UI.termGlyphMode === 'auto'
    ? 'Nerd Font ✓'
    : (eff === 'compat' ? L('图标: 兼容', 'Icons: Compat') : L('图标: 原始', 'Icons: Raw'));
  const cls = `glyph-pill${native && eff === 'raw' ? ' native' : eff === 'compat' ? ' on' : ''}`;
  const title = eff === 'compat'
    ? L('当前自动将 Nerd Font / Powerline 私有图标转为通用 Unicode（避免乱码方块）；点击切换原始字符', 'Translating Nerd Font / Powerline glyphs to standard Unicode to prevent missing glyph boxes; click to toggle raw glyphs')
    : (native ? L('已检测到本地 Nerd Font，正在原生渲染图标；点击可切到通用兼容图标', 'Local Nerd Font detected — rendering native glyphs; click to switch to compatible Unicode glyphs') : L('正在显示终端原始字符（若出现方块乱码，点击切回兼容图标模式）', 'Showing raw terminal glyphs (click to switch back to compatible mode if boxes appear)'));
  return h('button', { type: 'button', class: cls, title, onclick: cycleGlyphMode }, label);
}

function buildRawToolbar(pane, curMode, actualLines, pv) {
  const key = pvKeyOf(pane, curMode);
  const isLoading = S.previews.get(key)?.state === 'loading';
  const totalLines = pv?.totalLines || actualLines;
  const statusText = curMode === 'all'
    ? L(`显示全部 ${actualLines} 行${pv?.truncated ? '（已达 4 MiB 安全上限）' : ''}`, `Showing all ${actualLines} lines${pv?.truncated ? ' (hit 4 MiB safety limit)' : ''}`)
    : (totalLines > actualLines ? L(`显示 ${actualLines} / 共 ${totalLines} 行`, `Showing ${actualLines} / ${totalLines} lines`) : L(`显示 ${actualLines} 行`, `Showing ${actualLines} lines`));
  return h('div', { class: 'raw-toolbar' },
    h('div', { class: 'row-nowrap', style: 'gap:8px;min-width:0' },
      span('muted tabular', statusText),
      pv?.staleErr ? span('tag soft-amber', L('缓存', 'Cached')) : null,
      pv?.paginated ? span('tag soft-blue', L('已暂停自动滚动', 'Auto-scroll paused')) : null),
    h('div', { class: 'row-nowrap', style: 'gap:6px;flex-wrap:wrap' },
      h('div', { class: 'view-seg' }, PREVIEW_STEPS.map((step) => h('button', {
        type: 'button',
        class: curMode === step ? 'on' : '',
        disabled: isLoading && curMode !== step,
        onclick: () => {
          UI.pvLines.set(pane, step);
          loadPreview(pane, { mode: step, silent: false });
          APP.schedule();
        },
      }, step === 'all' ? L('全部历史', 'All history') : L(`${step} 行`, `${step} lines`)))),
      buildGlyphTogglePill(),
      btn(L('刷新', 'Refresh'), { cls: 'xs', onclick: () => loadPreview(pane, { mode: curMode, silent: false }) })));
}

function buildRawOlderBar(pane, pv) {
  if (!pv || pv.state !== 'ok' || !pv.hasMore || !(pv.nextBeforeLine > 0)) return null;
  const remaining = pv.nextBeforeLine;
  return h('div', { class: 'history-bar' },
    span('muted tabular', L(`上方还有 ${remaining} 行更早的终端滚动历史`, `${remaining} earlier scrollback lines above`)),
    h('div', { class: 'row-nowrap', style: 'gap:6px' },
      btn(L(`加载更早 (+${Math.min(PREVIEW_PAGE_STEP, remaining)} 行)`, `Load earlier (+${Math.min(PREVIEW_PAGE_STEP, remaining)} lines)`), {
        cls: 'xs',
        busyKey: `pvmore:${pane}`,
        onclick: () => loadEarlierPreview(pane, PREVIEW_PAGE_STEP),
      }),
      btn(L('加载全部历史', 'Load all history'), {
        cls: 'ghost xs',
        onclick: () => {
          UI.pvLines.set(pane, 'all');
          loadPreview(pane, { mode: 'all', silent: false });
          APP.schedule();
        },
      })));
}

function buildHistoryBar(s, tr) {
  if (!tr || tr.state !== 'ok' || tr.source !== 'transcript') return null;
  const shown = tr.messages?.length || 0;
  const total = tr.totalMessages || shown;
  const pausedTag = tr.autoPollPaused
    ? span('tag soft-blue', L('自动刷新已暂停', 'Auto-refresh paused'))
    : null;
  const refreshLatestBtn = tr.autoPollPaused
    ? btn(L('刷新最新', 'Refresh'), {
      cls: 'xs',
      onclick: () => loadTranscript(s.session_id, { silent: false, all: Boolean(tr.explicitAll) }),
    })
    : null;
  const compactBtn = (shown > TRANSCRIPT_LIMIT)
    ? btn(L(`收起至最近 ${TRANSCRIPT_LIMIT} 条`, `Compact to latest ${TRANSCRIPT_LIMIT}`), {
      cls: 'ghost xs',
      onclick: () => APP.compactTranscript(s.session_id),
    })
    : null;
  if (tr.hasMore) {
    const earlier = tr.nextBefore ?? tr.startSeq ?? Math.max(0, total - shown);
    return h('div', { class: 'history-bar' },
      h('div', { class: 'row-nowrap', style: 'gap:8px;flex-wrap:wrap' },
        span('muted tabular', L(`已显示最近 ${shown} / 共 ${total} 条对话（上方还有 ${earlier} 条）`, `Showing latest ${shown} / ${total} messages (${earlier} earlier above)`)),
        pausedTag),
      h('div', { class: 'row-nowrap', style: 'gap:6px;flex-wrap:wrap' },
        refreshLatestBtn,
        btn(L(`加载更早 (+${Math.min(TRANSCRIPT_PAGE_STEP, earlier)})`, `Load earlier (+${Math.min(TRANSCRIPT_PAGE_STEP, earlier)})`), {
          cls: 'xs',
          busyKey: `trmore:${s.session_id}`,
          onclick: () => loadEarlierTranscript(s.session_id, TRANSCRIPT_PAGE_STEP),
        }),
        btn(L(`加载全部 (${total})`, `Load all (${total})`), {
          cls: 'ghost xs',
          busyKey: `trall:${s.session_id}`,
          onclick: () => loadAllTranscript(s.session_id),
        }),
        compactBtn));
  }
  if (shown > 120 || (tr.explicitAll && shown > TRANSCRIPT_LIMIT) || tr.autoPollPaused) {
    return h('div', { class: 'history-bar' },
      h('div', { class: 'row-nowrap', style: 'gap:8px;flex-wrap:wrap' },
        span('muted tabular', L(`已加载全部 ${shown} 条历史对话`, `Showing all ${shown} messages`)),
        pausedTag),
      h('div', { class: 'row-nowrap', style: 'gap:6px;flex-wrap:wrap' },
        refreshLatestBtn,
        compactBtn));
  }
  return null;
}

function parseSystemCardText(rawText) {
  const txt = String(rawText || '').trim();
  const nl = txt.indexOf('\n');
  const firstLine = (nl >= 0 ? txt.slice(0, nl) : txt).trim();
  const rest = (nl >= 0 ? txt.slice(nl + 1) : '').trim();
  let tag = L('系统消息', 'System');
  let body = txt;
  if (firstLine.startsWith('[后台任务') && firstLine.endsWith(']')) {
    const inner = firstLine.slice(1, -1);
    tag = L(inner, inner.replace(/^后台任务/, 'Background Task').replace(/：/g, ': '));
    body = rest || txt;
  } else if (firstLine === '[Swarm 通知]') {
    tag = L('Swarm 通知', 'Swarm Notice');
    body = rest || txt;
  } else {
    const m = /^\[来自 ([^\]]+) 消息\]$/.exec(firstLine);
    if (m) {
      tag = `Agent · ${m[1]}`;
      body = rest || txt;
    }
  }
  const preview = body.replace(/\s+/g, ' ').trim();
  return { tag, preview, body };
}

const INTERMEDIATE_COLLAPSE_LINES = 6;
const INTERMEDIATE_COLLAPSE_CHARS = 280;
const LEGACY_MID_COLLAPSE_LINES = 8;
const LEGACY_MID_COLLAPSE_CHARS = 400;

const SWARM_FALLBACK_TOOLS = new Set(['swarm_wait', 'TeamCreate']);
const SUBAGENT_FALLBACK_TOOLS = new Set([
  'invoke_subagent',
  'run_workflow',
  'Agent',
  'Task',
  'SendMessage',
  'spawn_agent',
  'wait_agent',
  'resume_agent',
  'spawn_session',
  'create_handoff',
  'start_pipeline',
]);

function toolLeafName(name) {
  const raw = String(name || '').trim();
  if (!raw) return '';
  const afterDunder = raw.includes('__') ? raw.slice(raw.lastIndexOf('__') + 2) : raw;
  const afterDot = afterDunder.includes('.') ? afterDunder.slice(afterDunder.lastIndexOf('.') + 1) : afterDunder;
  return afterDot.includes(':') ? afterDot.slice(afterDot.lastIndexOf(':') + 1) : afterDot;
}

function delegationKindOf(msg) {
  if (!msg || msg.role !== 'assistant') return null;
  if (msg.delegation === 'swarm' || msg.delegation === 'subagent') return msg.delegation;
  if (!Array.isArray(msg.tools) || !msg.tools.length) return null;
  let foundSubagent = false;
  for (let i = 0; i < msg.tools.length; i++) {
    const leaf = toolLeafName(msg.tools[i]);
    if (!leaf) continue;
    if (SWARM_FALLBACK_TOOLS.has(leaf)) return 'swarm';
    if (SUBAGENT_FALLBACK_TOOLS.has(leaf)) foundSubagent = true;
  }
  return foundSubagent ? 'subagent' : null;
}

function buildDelegationPill(kind) {
  if (!kind) return null;
  const label = kind === 'swarm'
    ? L('Swarm 协作派发', 'Swarm Delegated')
    : L('子代理派发', 'Subagent Delegated');
  const title = kind === 'swarm'
    ? L('该回复已向 Swarm 协作组派发后台任务', 'This turn delegated background work to a Swarm worker')
    : L('该回复已派发子代理在后台执行任务', 'This turn delegated background work to a subagent');
  return h('span', { class: `msg-delegation-pill is-${kind}`, title },
    h('i', { class: 'msg-delegation-dot', 'aria-hidden': 'true' }),
    label);
}

function classifyCollapsibleAssistant(msg, hasLaterAssistantInTurn) {
  if (!msg || msg.role !== 'assistant') return null;
  const raw = String(msg.text || '');
  if (!raw.trim()) return null;
  const lines = raw.replace(/\r\n?/g, '\n').split('\n').length;
  const chars = raw.length;
  const kind = msg.kind || '';
  if (kind === 'plan') return { tag: L('执行计划', 'Plan'), lines };
  if (kind === 'agent_message') {
    return { tag: raw.trimStart().startsWith('[群组消息]') ? L('群组消息', 'Group Message') : L('代理通信', 'Agent Message'), lines };
  }
  if (kind === 'final') return null;
  if (kind === 'intermediate') {
    return lines >= INTERMEDIATE_COLLAPSE_LINES || chars >= INTERMEDIATE_COLLAPSE_CHARS
      ? { tag: L('过程输出', 'Intermediate'), lines }
      : null;
  }
  if (hasLaterAssistantInTurn && (lines >= LEGACY_MID_COLLAPSE_LINES || chars >= LEGACY_MID_COLLAPSE_CHARS)) {
    return { tag: L('过程输出', 'Intermediate'), lines };
  }
  return null;
}

function summarizeCollapsePreview(rawText) {
  const lines = String(rawText || '').replace(/\r\n?/g, '\n').split('\n');
  const cleaned = [];
  for (let i = 0; i < lines.length; i++) {
    let s = lines[i].trim();
    if (!s) continue;
    if (i === 0 && /^\[(?:群组消息|发送给 [^\]]+)\]$/.test(s)) continue;
    if (/^```/.test(s)) continue;
    s = s
      .replace(/^#{1,6}\s+/, '')
      .replace(/^>\s?/, '')
      .replace(/^[-*+]\s+/, '')
      .replace(/\*\*([^*]+)\*\*/g, '$1')
      .replace(/`([^`]+)`/g, '$1')
      .replace(/\s+/g, ' ')
      .trim();
    if (s) {
      cleaned.push(s);
      if (cleaned.length >= 2) break;
    }
  }
  if (!cleaned.length) return L('中间过程输出', 'Intermediate output');
  if (cleaned.length >= 2 && cleaned[0].length <= 24) return `${cleaned[0]} · ${cleaned[1]}`;
  return cleaned[0];
}

function renderMessageCard(s, msg, absSeq, userIdx = null, hasLaterAssistantInTurn = false, ctx = null) {
  if (msg.role === 'system') {
    const sysKey = `sys:${s.session_id}:${absSeq}:${msg.ts_ms || 0}`;
    const { tag, preview, body } = parseSystemCardText(msg.text);
    const initiallyOpen = UI.open.has(sysKey);
    let bodyRendered = false;
    const bodyWrap = body ? h('div', { class: 'msg-sys-body' }) : null;
    const hydrateSys = () => {
      if (bodyRendered || !bodyWrap) return;
      bodyRendered = true;
      bodyWrap.appendChild(renderMarkdown(body));
    };
    if (initiallyOpen) hydrateSys();
    const summaryEl = h('summary', { class: 'msg-sys-summary' },
      span('msg-sys-tag', tag),
      span('msg-sys-preview trunc grow', preview || L('系统通知', 'System notice')),
      msg.ts_ms ? span('tabular dim', clock(msg.ts_ms)) : null);
    const detailsEl = h('details', { class: 'msg-sys-details', 'data-k': sysKey, open: initiallyOpen },
      summaryEl,
      bodyWrap);
    if (bodyWrap && !initiallyOpen) {
      summaryEl.addEventListener('click', () => { if (!detailsEl.open) hydrateSys(); });
      detailsEl.addEventListener('toggle', () => { if (detailsEl.open) hydrateSys(); });
    }
    return h('article', { class: 'msg-card system' }, detailsEl);
  }
  const isUser = msg.role === 'user';
  const tools = Array.isArray(msg.tools) ? msg.tools.filter(Boolean) : [];
  const tKey = `tools:${s.session_id}:${absSeq}:${msg.ts_ms || 0}`;
  const agentName = ctx?.agentName || AGENT_DISPLAY[s.agent] || s.agent || 'Agent';
  const meLabel = ctx ? ctx.meLabel : modelEffortLabel(s.model, s.effort);
  const canQuote = !isUser && Boolean(msg.text && msg.text.trim()) && (ctx ? ctx.canQuoteBase : (!s.is_subagent && s.state !== 'ended'));
  let cardEl;
  const makeQuoteBtn = () => (canQuote ? h('button', {
    type: 'button',
    class: 'msg-quote-btn',
    title: L('引用选中的片段或整条回复到输入框（继续在其下方提问）', 'Quote selection or entire reply into composer to ask follow-up'),
    onmousedown: (e) => e.preventDefault(),
    onclick: () => {
      const sel = window.getSelection?.();
      let rawSel = '';
      if (sel && !sel.isCollapsed && sel.rangeCount > 0 && cardEl) {
        const rng = sel.getRangeAt(0);
        if (cardEl.contains(rng.commonAncestorContainer)) rawSel = sel.toString().trim();
      }
      quoteReplyToComposer(s.session_id, rawSel || msg.text);
    },
  }, L('引用', 'Quote')) : null);

  const delegation = isUser ? null : delegationKindOf(msg);
  const delCls = delegation ? ` is-delegation is-delegation-${delegation}` : '';
  const collapsible = classifyCollapsibleAssistant(msg, hasLaterAssistantInTurn);
  if (collapsible) {
    const midKey = `mid:${s.session_id}:${msg.ts_ms || absSeq}:${hashStr(String(msg.text || '').slice(0, 120))}`;
    const metaText = L(`${collapsible.lines} 行${tools.length ? ` · ${tools.length} 工具` : ''}`, `${collapsible.lines} lines${tools.length ? ` · ${tools.length} tool${tools.length === 1 ? '' : 's'}` : ''}`);
    const initiallyOpen = UI.open.has(midKey);
    const bodyEl = h('div', { class: 'msg-mid-body' });
    let bodyRendered = false;
    const summaryEl = h('summary', { class: 'msg-mid-summary' },
      h('i', { class: `dot ${delegation ? 'msg-delegation-dot' : `agent-dot-${s.agent || 'x'}`}` }),
      span('msg-mid-tag', collapsible.tag),
      span('msg-mid-preview trunc grow', summarizeCollapsePreview(msg.text)),
      span('msg-mid-meta tabular', metaText),
      msg.ts_ms ? span('tabular dim', clock(msg.ts_ms)) : null);
    const detailsEl = h('details', { class: 'msg-mid-details', 'data-k': midKey, open: initiallyOpen },
      summaryEl,
      bodyEl);
    const hydrateMid = () => {
      if (bodyRendered) return;
      bodyRendered = true;
      const collapseBtn = collapsible.lines >= 16 ? h('button', {
        type: 'button',
        class: 'msg-mid-collapse-btn',
        onclick: () => {
          detailsEl.open = false;
          UI.open.delete(midKey);
          detailsEl.scrollIntoView({ block: 'nearest' });
        },
      }, L(`收起${collapsible.tag}`, `Collapse ${collapsible.tag}`)) : null;
      bodyEl.append(
        h('div', { class: 'msg-mid-body-head' },
          h('span', { class: 'row' },
            span(`tag a-${s.agent || 'x'}`, agentName),
            meLabel ? span('mono muted', meLabel) : null,
            buildDelegationPill(delegation)),
          h('span', { class: 'row' }, makeQuoteBtn())),
        renderMarkdown(msg.text),
      );
      if (tools.length) {
        bodyEl.appendChild(h('details', { class: 'msg-tools', 'data-k': tKey, open: UI.open.has(tKey) },
          h('summary', null, L(`调用了 ${tools.length} 个工具`, `Used ${tools.length} tool${tools.length === 1 ? '' : 's'}`)),
          h('div', { class: 'msg-tool-list' }, tools.map((t) => span('tag mono', t)))));
      }
      if (collapseBtn) bodyEl.appendChild(collapseBtn);
    };
    if (initiallyOpen) hydrateMid();
    else {
      summaryEl.addEventListener('click', () => { if (!detailsEl.open) hydrateMid(); });
      detailsEl.addEventListener('toggle', () => { if (detailsEl.open) hydrateMid(); });
    }
    cardEl = h('article', {
      class: `msg-card intermediate${delCls}`,
      'data-delegation': delegation || null,
    }, detailsEl);
    return cardEl;
  }

  const turnIdxEl = isUser && userIdx != null ? span('turn-idx', `#${userIdx}`) : null;
  cardEl = h('article', {
    class: `msg-card ${isUser ? 'user' : 'assistant'}${delCls}`,
    id: isUser && userIdx != null ? `turn-u-${userIdx}` : null,
    'data-delegation': delegation || null,
  },
  h('div', { class: 'msg-head' },
    isUser
      ? h('span', { class: 'row' }, span('msg-user-tag', L('你', 'You')), turnIdxEl)
      : h('span', { class: 'row' },
        h('i', { class: `dot agent-dot-${s.agent || 'x'}` }),
        span(`tag a-${s.agent || 'x'}`, agentName),
        meLabel ? span('mono muted', meLabel) : null,
        buildDelegationPill(delegation)),
    h('span', { class: 'row' }, makeQuoteBtn(), msg.ts_ms ? span('tabular dim', clock(msg.ts_ms)) : null)),
  msg.text
    ? renderMarkdown(msg.text)
    : (delegation
      ? h('div', { class: 'msg-delegation-pending' },
        h('i', { class: 'msg-delegation-dot', 'aria-hidden': 'true' }),
        delegation === 'swarm'
          ? L('已派发 Swarm 协作任务，正在后台执行…', 'Delegated to Swarm worker, running in background…')
          : L('已派发子代理，正在后台执行…', 'Delegated to subagent, running in background…'))
      : (tools.length ? h('div', { class: 'muted', style: 'font-size:13px' }, L('正在调用工具…', 'Calling tools…')) : null)),
  tools.length ? h('details', { class: 'msg-tools', 'data-k': tKey, open: UI.open.has(tKey) },
    h('summary', null, L(`调用了 ${tools.length} 个工具`, `Used ${tools.length} tool${tools.length === 1 ? '' : 's'}`)),
    h('div', { class: 'msg-tool-list' }, tools.map((t) => span('tag mono', t)))) : null);
  if (turnIdxEl) cardEl._turnIdxEl = turnIdxEl;
  return cardEl;
}

function renderTimelineCards(s, list, startSeq = 0) {
  const hasLaterAssistant = new Array(list.length).fill(false);
  let seenAssistantWithText = false;
  for (let i = list.length - 1; i >= 0; i--) {
    const m = list[i];
    if (m.role === 'user') {
      seenAssistantWithText = false;
    } else if (m.role === 'assistant') {
      hasLaterAssistant[i] = seenAssistantWithText;
      if (String(m.text || '').trim()) seenAssistantWithText = true;
    }
  }
  const ctx = {
    agentName: AGENT_DISPLAY[s.agent] || s.agent || 'Agent',
    meLabel: modelEffortLabel(s.model, s.effort),
    canQuoteBase: !s.is_subagent && s.state !== 'ended',
  };
  let uIdx = 0;
  return list.map((msg, idx) => renderMessageCard(
    s,
    msg,
    startSeq + idx,
    msg.role === 'user' ? ++uIdx : null,
    hasLaterAssistant[idx],
    ctx,
  ));
}

function syncChildrenMinimal(parent, desired) {
  const cur = parent.children;
  const curLen = cur.length;
  const wantLen = desired.length;
  if (curLen === wantLen) {
    let diffIdx = -1;
    let diffCount = 0;
    for (let i = 0; i < wantLen; i++) {
      if (cur[i] !== desired[i]) {
        diffIdx = i;
        diffCount++;
        if (diffCount > 1) break;
      }
    }
    if (diffCount === 0) return;
    if (diffCount === 1) {
      cur[diffIdx].replaceWith(desired[diffIdx]);
      return;
    }
  } else if (wantLen > curLen && curLen > 0) {
    // Fast pure-append check (streaming / new turns at bottom).
    let isAppend = true;
    for (let i = 0; i < curLen; i++) {
      if (cur[i] !== desired[i]) { isAppend = false; break; }
    }
    if (isAppend) {
      const frag = document.createDocumentFragment();
      for (let i = curLen; i < wantLen; i++) frag.appendChild(desired[i]);
      parent.appendChild(frag);
      return;
    }
    // Fast pure-prepend check ("Load earlier" at top).
    const offset = wantLen - curLen;
    let isPrepend = true;
    for (let i = 0; i < curLen; i++) {
      if (cur[i] !== desired[offset + i]) { isPrepend = false; break; }
    }
    if (isPrepend) {
      const frag = document.createDocumentFragment();
      for (let i = 0; i < offset; i++) frag.appendChild(desired[i]);
      parent.prepend(frag);
      return;
    }
  }
  parent.replaceChildren(...desired);
}

function optimisticTurnsFor(sid) {
  return (sid && UI.optimisticTurns.get(sid)) || [];
}

function optimisticSigOf(sid, lang) {
  const list = optimisticTurnsFor(sid);
  if (!list.length) return '';
  return `${lang}:${list.map((o) => `${o.nonce}:${o.status}`).join(',')}`;
}

function renderOptimisticCard(entry) {
  const isSending = entry.status === 'sending';
  const statusLabel = isSending
    ? L('发送中…', 'Sending…')
    : L('已送达 · 思考中…', 'Delivered · Thinking…');
  const atts = Array.isArray(entry.attachments) ? entry.attachments : [];
  return h('article', {
    class: `msg-card user is-optimistic is-opt-${entry.status}`,
    'data-opt-nonce': entry.nonce,
  },
  h('div', { class: 'msg-head' },
    h('span', { class: 'row' },
      span('msg-user-tag', L('你', 'You')),
      h('span', { class: `msg-opt-chip${isSending ? ' is-sending' : ' is-sent'}` },
        h('i', { class: `msg-opt-dot${isSending ? ' is-sending' : ' is-sent'}`, 'aria-hidden': 'true' }),
        statusLabel)),
    entry.created_ms ? span('tabular dim', clock(entry.created_ms)) : null),
  entry.text ? renderMarkdown(entry.text) : null,
  atts.length ? h('div', { class: 'msg-opt-atts' }, atts.map((a) => span(
    'tag mono',
    a.token || a.title || (a.kind === 'image' ? L('图片', 'Image') : L('粘贴片段', 'Paste')),
  ))) : null);
}

function renderOptimisticCards(sid) {
  const list = optimisticTurnsFor(sid);
  return list.length ? list.map(renderOptimisticCard) : [];
}

function resolveLiveTailSource(s, tr, pv) {
  const trTail = typeof tr?.live_tail === 'string'
    ? { text: tr.live_tail, hash: hashStr(tr.live_tail), updatedMs: tr.liveTailUpdatedMs || tr.updatedMs || 0 }
    : null;
  const mapTail = S.liveTails.get(s.session_id) || null;
  const sseTail = trTail && mapTail
    ? (trTail.updatedMs >= mapTail.updatedMs ? trTail : mapTail)
    : (trTail || mapTail);
  const useSse = Boolean(sseTail && typeof sseTail.text === 'string' && (!pv?.text || sseTail.updatedMs >= (pv.updatedMs || 0)));
  if (useSse) return { rawText: sseTail.text, hashPart: `sse:${sseTail.hash}` };
  return { rawText: pv?.text || '', hashPart: pv?.hash || '' };
}

function createTimelineView(sid) {
  const alertSlot = h('div', { class: 'timeline-slot timeline-alert-slot' });
  const historySlot = h('div', { class: 'timeline-slot timeline-history-slot' });
  const chunksEl = h('div', { class: 'timeline-chunks' });
  const optimisticSlot = h('div', { class: 'timeline-slot timeline-optimistic-slot' });
  const tailSlot = h('div', { class: 'timeline-slot timeline-tail-slot' });
  const root = h('div', { class: 'timeline' }, alertSlot, historySlot, chunksEl, optimisticSlot, tailSlot);
  return {
    sid,
    root,
    alertSlot,
    historySlot,
    chunksEl,
    optimisticSlot,
    tailSlot,
    chunkMap: new Map(),
    chunkOrder: [],
    errSig: '',
    historySig: '',
    messagesSig: '',
    optSig: '',
    tailSig: '',
    tailKey: '',
    tailDetails: null,
    tailSummary: null,
    tailPre: null,
    prevAutoOpenTail: false,
  };
}

function reconcileTimelineChunks(tv, s, tr) {
  const list = tr.messages;
  const startSeq = Number.isFinite(tr.startSeq) ? tr.startSeq : 0;
  const n = list.length;
  const hasLaterAssistant = new Array(n).fill(false);
  let seenAssistantWithText = false;
  for (let i = n - 1; i >= 0; i--) {
    const m = list[i];
    if (m.role === 'user') {
      seenAssistantWithText = false;
    } else if (m.role === 'assistant') {
      hasLaterAssistant[i] = seenAssistantWithText;
      if (String(m.text || '').trim()) seenAssistantWithText = true;
    }
  }
  const ctx = {
    agentName: AGENT_DISPLAY[s.agent] || s.agent || 'Agent',
    meLabel: modelEffortLabel(s.model, s.effort),
    canQuoteBase: !s.is_subagent && s.state !== 'ended',
  };
  const lang = getLang();
  const cardCtxSig = `${ctx.canQuoteBase ? 1 : 0}|${s.agent}|${ctx.meLabel}|${lang}`;
  const chunkSpecs = new Map();
  const chunkOrder = [];
  let uIdx = 0;
  for (let i = 0; i < n; i++) {
    const msg = list[i];
    const absSeq = startSeq + i;
    const chunkId = Math.floor(absSeq / TIMELINE_CHUNK_SIZE);
    let arr = chunkSpecs.get(chunkId);
    if (!arr) {
      arr = [];
      chunkSpecs.set(chunkId, arr);
      chunkOrder.push(chunkId);
    }
    const userIdx = msg.role === 'user' ? ++uIdx : null;
    const hasLater = hasLaterAssistant[i];
    const effectiveHasLater = msg.role === 'assistant' && !msg.kind && hasLater;
    const cardSig = `${hashMessage(msg)}|${delegationKindOf(msg) || ''}|${effectiveHasLater ? 1 : 0}|${cardCtxSig}`;
    arr.push({ absSeq, msg, userIdx, hasLater, cardSig });
  }

  for (const [cid, entry] of [...tv.chunkMap.entries()]) {
    if (!chunkSpecs.has(cid)) {
      entry.el.remove();
      tv.chunkMap.delete(cid);
    }
  }

  const desiredChunkEls = [];
  for (let ci = 0; ci < chunkOrder.length; ci++) {
    const cid = chunkOrder[ci];
    const specs = chunkSpecs.get(cid);
    let entry = tv.chunkMap.get(cid);
    if (!entry) {
      entry = { el: h('div', { class: 'timeline-chunk', 'data-chunk-id': String(cid) }), cards: new Map(), userTurns: [] };
      tv.chunkMap.set(cid, entry);
    }
    const nextCardsMap = new Map();
    const desiredCards = [];
    const userTurns = [];
    for (let j = 0; j < specs.length; j++) {
      const sp = specs[j];
      let cardEl = entry.cards.get(sp.absSeq);
      if (cardEl && cardEl._cardSig === sp.cardSig) {
        if (sp.userIdx != null && cardEl._userIdx !== sp.userIdx) {
          cardEl.id = `turn-u-${sp.userIdx}`;
          if (cardEl._turnIdxEl) cardEl._turnIdxEl.textContent = `#${sp.userIdx}`;
          cardEl._userIdx = sp.userIdx;
        }
      } else {
        cardEl = renderMessageCard(s, sp.msg, sp.absSeq, sp.userIdx, sp.hasLater, ctx);
        cardEl._cardSig = sp.cardSig;
        cardEl._absSeq = sp.absSeq;
        cardEl._userIdx = sp.userIdx;
      }
      nextCardsMap.set(sp.absSeq, cardEl);
      desiredCards.push(cardEl);
      if (sp.userIdx != null) userTurns.push({ idx: sp.userIdx, el: cardEl });
    }
    entry.cards = nextCardsMap;
    entry.userTurns = userTurns;
    syncChildrenMinimal(entry.el, desiredCards);
    desiredChunkEls.push(entry.el);
  }
  tv.chunkOrder = chunkOrder;
  syncChildrenMinimal(tv.chunksEl, desiredChunkEls);
}

function patchLiveTail(tv, s, rawTailText, autoOpenTail, tailSig) {
  tv.tailSig = tailSig;
  const tailText = rawTailText ? extractLiveTail(rawTailText, LIVE_TAIL_LINES) : '';
  const tailKey = `tail:${s.session_id}`;
  if (!tailText) {
    if (tv.tailDetails) {
      tv.tailSlot.replaceChildren();
      tv.tailDetails = null;
      tv.tailSummary = null;
      tv.tailPre = null;
      tv.tailKey = '';
    }
    tv.prevAutoOpenTail = autoOpenTail;
    return;
  }
  const summaryLabel = L(
    `当前终端实时行（${paneDisplayId(s) || 'pane'} · 最近 ${LIVE_TAIL_LINES} 行）`,
    `Live terminal tail (${paneDisplayId(s) || 'pane'} · last ${LIVE_TAIL_LINES} lines)`,
  );
  if (tv.tailDetails && tv.tailKey === tailKey && tv.tailDetails.isConnected) {
    // In-place textContent patch: zero rebuild of timeline message cards or <details> element!
    if (tv.tailSummary.textContent !== summaryLabel) tv.tailSummary.textContent = summaryLabel;
    if (tv.tailPre.textContent !== tailText) tv.tailPre.textContent = tailText;
    if (autoOpenTail && !tv.prevAutoOpenTail && !tv.tailDetails.open) tv.tailDetails.open = true;
    tv.prevAutoOpenTail = autoOpenTail;
    return;
  }
  const summaryEl = h('summary', null, summaryLabel);
  const preEl = h('pre', { class: 'term-pre' }, tailText);
  const detailsEl = h('details', {
    class: 'live-tail',
    'data-k': tailKey,
    open: UI.open.has(tailKey) || autoOpenTail,
  }, summaryEl, preEl);
  tv.tailSlot.replaceChildren(detailsEl);
  tv.tailDetails = detailsEl;
  tv.tailSummary = summaryEl;
  tv.tailPre = preEl;
  tv.tailKey = tailKey;
  tv.prevAutoOpenTail = autoOpenTail;
}

function computeNonTranscriptSig(s, mode, pane, curPvMode, pv, tr, glyphSig, errSig, optSig, lang) {
  const staleSig = pv?.staleErr ? hashStr(pv.staleErr) : '';
  if (mode === 'raw') {
    if (!pane) return `raw:nopane:${s.session_id}:${errSig}:${lang}`;
    if (!pv || (pv.state === 'loading' && !pv.text)) return `raw:loading:${pane}:${curPvMode}:${errSig}:${lang}`;
    if (pv.state === 'err' && !pv.text) return `raw:err:${pane}:${curPvMode}:${pv.err}:${errSig}:${lang}`;
    if (!pv.text.trim()) return `raw:empty:${pane}:${curPvMode}:${errSig}:${lang}`;
    return `raw:ok:${pane}:${curPvMode}:${pv.hash}:${staleSig}:${glyphSig}:${errSig}:${UI.busy.has(`pvmore:${pane}`) ? 1 : 0}:${lang}`;
  }
  if (!pane) {
    if (tr?.state === 'loading' && !optSig) return `read:trload:${s.session_id}:${errSig}:${lang}`;
    if (s.last_message) return `read:last:${s.session_id}:${hashStr(s.last_message)}:${errSig}:${optSig}:${lang}`;
    return `read:empty:${s.session_id}:${errSig}:${optSig}:${lang}`;
  }
  if ((!pv || (pv.state === 'loading' && !pv.text)) && (!tr || tr.state === 'loading') && !optSig) {
    return `read:loading:${s.session_id}:${curPvMode}:${errSig}:${lang}`;
  }
  if (pv?.state === 'err' && !pv.text) return `read:err:${pane}:${curPvMode}:${pv.err}:${errSig}:${optSig}:${lang}`;
  if (!pv?.text?.trim()) return `read:empty:${pane}:${curPvMode}:${errSig}:${optSig}:${lang}`;
  return `read:pv:${pane}:${curPvMode}:${pv.hash}:${staleSig}:${glyphSig}:${errSig}:${UI.busy.has(`pvmore:${pane}`) ? 1 : 0}:${optSig}:${lang}`;
}

export function buildOutputBody(s) {
  const mode = s.agent === 'shell' ? 'raw' : UI.outputMode;
  const pane = s.pane;
  const curPvMode = pane ? pvModeOf(pane) : PREVIEW_STEPS[0];
  const pvKey = pane ? pvKeyOf(pane, curPvMode) : null;
  const pv = pvKey ? S.previews.get(pvKey) : null;
  const tr = S.transcripts.get(s.session_id);
  const glyphSig = `${UI.termGlyphMode}:${effectiveGlyphMode()}`;
  const lang = getLang();
  const hid = hostOf(s);
  const hStatus = isRemoteHost(hid) ? (S.hosts.get(hid)?.status || 'online') : 'online';
  const errSig = `${s.state}:${hashStr(s.last_error || '')}:${UI.busy.has(`ack:${s.session_id}`) ? 1 : 0}:${hStatus}:${UI.busy.has(`probe-host:${hid}`) ? 1 : 0}`;
  const optSig = mode === 'reading' ? optimisticSigOf(s.session_id, lang) : '';
  const optCards = optSig ? renderOptimisticCards(s.session_id) : [];
  const alertBanner = sessionAlertBanner(s);
  const pvStaleBanner = pv?.staleErr
    ? buildStaleBanner(s, pv.staleErr, () => loadPreview(pane, { mode: curPvMode, silent: false }))
    : null;
  const staleSig = pv?.staleErr ? hashStr(pv.staleErr) : '';

  if (mode === 'raw') {
    if (!pane) {
      return {
        sig: `raw:nopane:${s.session_id}:${errSig}:${lang}`,
        sourceLabel: L('原始终端', 'Raw terminal'),
        node: h('div', { class: 'raw-shell stack-sm' }, alertBanner, emptyState(L('该会话未绑定终端 pane', 'No terminal pane attached'), s.last_message || L('无法抓屏预览原始终端。', 'Cannot capture raw terminal output.'))),
      };
    }
    if (!pv || (pv.state === 'loading' && !pv.text)) {
      return {
        sig: `raw:loading:${pane}:${curPvMode}:${errSig}:${lang}`,
        sourceLabel: L('原始终端', 'Raw terminal'),
        node: h('div', { class: 'raw-shell stack-sm' }, alertBanner, skeleton(6)),
      };
    }
    if (pv.state === 'err' && !pv.text) {
      return {
        sig: `raw:err:${pane}:${curPvMode}:${pv.err}:${errSig}:${lang}`,
        sourceLabel: L('原始终端', 'Raw terminal'),
        node: h('div', { class: 'raw-shell stack-sm' },
          alertBanner,
          buildRawToolbar(pane, curPvMode, 0, pv),
          errorBanner(L(`读取终端失败：${pv.err}`, `Failed to read terminal: ${pv.err}`), L('重试获取终端', 'Retry terminal'), () => loadPreview(pane, { mode: curPvMode, silent: false })),
          s.last_message ? h('div', { class: 'callout' }, h('div', { class: 'sec-h' }, L('最近事件摘要', 'Latest summary')), renderMarkdown(s.last_message)) : null),
      };
    }
    if (!pv.text.trim()) {
      return {
        sig: `raw:empty:${pane}:${curPvMode}:${errSig}:${lang}`,
        sourceLabel: L('原始终端', 'Raw terminal'),
        node: h('div', { class: 'raw-shell stack-sm' }, alertBanner, buildRawToolbar(pane, curPvMode, 0, pv), emptyState(L('终端暂无可见输出', 'No visible terminal output'), s.last_message || L('向下方输入框发送一条消息试试。', 'Try sending a message from the composer below.'))),
      };
    }
    const cleanRaw = formatTerminalText(pv.text);
    const actualLines = cleanRaw ? cleanRaw.split('\n').length : 0;
    return {
      sig: `raw:ok:${pane}:${curPvMode}:${pv.hash}:${staleSig}:${glyphSig}:${errSig}:${UI.busy.has(`pvmore:${pane}`) ? 1 : 0}:${lang}`,
      sourceLabel: L('原始终端', 'Raw terminal'),
      node: h('div', { class: 'raw-shell stack-sm' },
        alertBanner,
        buildRawToolbar(pane, curPvMode, actualLines, pv),
        pvStaleBanner,
        buildRawOlderBar(pane, pv),
        renderChunkedTerminal(cleanRaw)),
    };
  }

  if (tr?.state === 'ok' && tr.source === 'transcript' && tr.messages.length > 0) {
    const tv = createTimelineView(s.session_id);
    const trStaleErr = tr.staleErr || pv?.staleErr || '';
    const trStaleBanner = trStaleErr
      ? buildStaleBanner(s, trStaleErr, () => loadSessionOutput(s, { silent: false, forcePreview: Boolean(pv?.staleErr), force: true }))
      : null;
    const alertNodes = [alertBanner, trStaleBanner].filter(Boolean);
    if (alertNodes.length) tv.alertSlot.replaceChildren(...alertNodes);
    const hb = buildHistoryBar(s, tr);
    if (hb) tv.historySlot.replaceChildren(hb);
    reconcileTimelineChunks(tv, s, tr);
    if (optCards.length) tv.optimisticSlot.replaceChildren(...optCards);
    const tailSrc = resolveLiveTailSource(s, tr, pv);
    patchLiveTail(tv, s, tailSrc.rawText, sessionNeedsLiveTail(s, tr), `${tailSrc.hashPart}:${glyphSig}`);
    return {
      sig: `read:tr:${s.session_id}:${tr.hash}:${hashStr(trStaleErr)}:${errSig}:${optSig}:${lang}`,
      sourceLabel: L('对话记录', 'Transcript'),
      node: tv.root,
    };
  }

  if (!pane) {
    if (tr?.state === 'loading' && !optCards.length) return { sig: `read:trload:${s.session_id}:${errSig}:${lang}`, sourceLabel: L('阅读视图', 'Reading view'), node: h('div', { class: 'timeline' }, alertBanner, skeleton(5)) };
    if (s.last_message) return { sig: `read:last:${s.session_id}:${hashStr(s.last_message)}:${errSig}:${optSig}:${lang}`, sourceLabel: L('最近摘要', 'Latest summary'), node: h('div', { class: 'timeline' }, alertBanner, renderMessageCard(s, { role: 'assistant', text: s.last_message }, 0), ...optCards) };
    if (optCards.length) return { sig: `read:empty:${s.session_id}:${errSig}:${optSig}:${lang}`, sourceLabel: L('阅读视图', 'Reading view'), node: h('div', { class: 'timeline' }, alertBanner, ...optCards) };
    return { sig: `read:empty:${s.session_id}:${errSig}:${lang}`, sourceLabel: L('阅读视图', 'Reading view'), node: h('div', { class: 'timeline' }, alertBanner, emptyState(L('暂无可阅读内容', 'Nothing to read yet'), L('该会话尚未产生对话记录且未绑定终端 pane。', 'This session has no transcript yet and no terminal pane attached.'))) };
  }
  if ((!pv || (pv.state === 'loading' && !pv.text)) && (!tr || tr.state === 'loading') && !optCards.length) {
    return { sig: `read:loading:${s.session_id}:${curPvMode}:${errSig}:${lang}`, sourceLabel: L('阅读视图', 'Reading view'), node: h('div', { class: 'timeline' }, alertBanner, skeleton(6)) };
  }
  if (pv?.state === 'err' && !pv.text) {
    return {
      sig: `read:err:${pane}:${curPvMode}:${pv.err}:${errSig}:${optSig}:${lang}`,
      sourceLabel: L('终端 Markdown', 'Terminal Markdown'),
      node: h('div', { class: 'timeline' },
        alertBanner,
        errorBanner(L(`读取终端失败：${pv.err}`, `Failed to read terminal: ${pv.err}`), L('重试获取终端', 'Retry terminal'), () => loadSessionOutput(s, { silent: false, forcePreview: true, force: true })),
        s.last_message ? renderMessageCard(s, { role: 'assistant', text: s.last_message }, 0) : null,
        ...optCards),
    };
  }
  if (!pv?.text?.trim()) {
    if (optCards.length) {
      return { sig: `read:empty:${pane}:${curPvMode}:${errSig}:${optSig}:${lang}`, sourceLabel: L('阅读视图', 'Reading view'), node: h('div', { class: 'timeline' }, alertBanner, ...optCards) };
    }
    return { sig: `read:empty:${pane}:${curPvMode}:${errSig}:${lang}`, sourceLabel: L('终端 Markdown', 'Terminal Markdown'), node: h('div', { class: 'timeline' }, alertBanner, emptyState(L('终端暂无可见输出', 'No visible terminal output'), s.last_message || L('向下方输入框发送一条消息试试。', 'Try sending a message from the composer below.'))) };
  }
  const turns = parseTerminalTurns(pv.text, s.agent);
  const fallbackToolbar = buildRawToolbar(pane, curPvMode, pv.text.split('\n').length, pv);
  const pvSig = `read:pv:${pane}:${curPvMode}:${pv.hash}:${staleSig}:${glyphSig}:${errSig}:${UI.busy.has(`pvmore:${pane}`) ? 1 : 0}:${optSig}:${lang}`;
  if (!turns.length) {
    return {
      sig: pvSig,
      sourceLabel: L('终端 Markdown', 'Terminal Markdown'),
      node: h('div', { class: 'timeline' },
        alertBanner,
        fallbackToolbar,
        pvStaleBanner,
        buildRawOlderBar(pane, pv),
        h('article', { class: 'msg-card assistant' }, renderChunkedTerminal(formatTerminalText(pv.text))),
        ...optCards),
    };
  }
  return {
    sig: pvSig,
    sourceLabel: L('终端 Markdown', 'Terminal Markdown'),
    node: h('div', { class: 'timeline' }, alertBanner, fallbackToolbar, pvStaleBanner, buildRawOlderBar(pane, pv), renderTimelineCards(s, turns), ...optCards),
  };
}

/**
 * Signature-first, decoupled output viewport synchronizer:
 * - In structured transcript mode, `.timeline-chunks`, `.timeline-optimistic-slot`, and `.live-tail`
 *   are completely decoupled; tail ticks patch `<pre>.textContent` in-place with ZERO `.msg-card`
 *   destruction or Markdown parsing.
 * - Message updates reconcile only changed/appended/prepended cards inside 20-card `.timeline-chunk`s.
 * - In raw/fallback modes, computes signature BEFORE building any DOM so unchanged renders are O(1).
 */
function syncOutputViewport(vp, s) {
  const mode = s.agent === 'shell' ? 'raw' : UI.outputMode;
  const pane = s.pane;
  const curPvMode = pane ? pvModeOf(pane) : PREVIEW_STEPS[0];
  const pvKey = pane ? pvKeyOf(pane, curPvMode) : null;
  const pv = pvKey ? S.previews.get(pvKey) : null;
  const tr = S.transcripts.get(s.session_id);
  const glyphSig = `${UI.termGlyphMode}:${effectiveGlyphMode()}`;
  const lang = getLang();
  const hid = hostOf(s);
  const hStatus = isRemoteHost(hid) ? (S.hosts.get(hid)?.status || 'online') : 'online';
  const baseErrSig = `${s.state}:${hashStr(s.last_error || '')}:${UI.busy.has(`ack:${s.session_id}`) ? 1 : 0}:${hStatus}:${UI.busy.has(`probe-host:${hid}`) ? 1 : 0}:${lang}`;
  const optSig = mode === 'reading' ? optimisticSigOf(s.session_id, lang) : '';
  const sameView = vp.dataset.sid === s.session_id && vp.dataset.mode === mode;
  const isTranscript = mode === 'reading' && tr?.state === 'ok' && tr.source === 'transcript' && tr.messages.length > 0;
  const forceBottom = UI.forceScrollBottomSid === s.session_id;
  if (forceBottom) UI.forceScrollBottomSid = null;

  if (isTranscript) {
    const autoOpenTail = sessionNeedsLiveTail(s, tr);
    const tailOpen = UI.open.has(`tail:${s.session_id}`) || autoOpenTail;
    const staleErr = tr.staleErr || (tailOpen ? (pv?.staleErr || '') : '');
    const errSig = `${baseErrSig}:${hashStr(staleErr)}`;
    const tailSrc = resolveLiveTailSource(s, tr, pv);
    const tailSig = `${tailSrc.hashPart}:${glyphSig}:${autoOpenTail ? 1 : 0}:${paneDisplayId(s)}:${lang}`;
    const historySig = `${tr.startSeq}:${tr.totalMessages}:${tr.messages.length}:${tr.hasMore ? 1 : 0}:${tr.explicitAll ? 1 : 0}:${tr.autoPollPaused ? 1 : 0}:${tr.latestTotalMessages || 0}:${UI.busy.has(`trmore:${s.session_id}`) ? 1 : 0}:${UI.busy.has(`trall:${s.session_id}`) ? 1 : 0}:${lang}`;
    let delSig = '';
    for (let i = 0; i < tr.messages.length; i++) {
      const dk = tr.messages[i]?.delegation;
      if (dk) delSig += `${i}:${dk},`;
    }
    const messagesSig = `${s.session_id}:${s.agent}:${s.model || ''}:${s.effort || ''}:${s.is_subagent ? 1 : 0}:${s.state === 'ended' ? 1 : 0}:${tr.hash}:${delSig}:${lang}`;
    const fullSig = `read:tr:${messagesSig}:${optSig}`;
    const renderAlertNodes = () => [
      sessionAlertBanner(s),
      staleErr ? buildStaleBanner(s, staleErr, () => loadSessionOutput(s, { silent: false, forcePreview: Boolean(pv?.staleErr), force: true })) : null,
    ].filter(Boolean);

    const tv = vp._tv;
    if (sameView && tv && tv.sid === s.session_id && tv.root.isConnected) {
      if (tv.errSig === errSig && tv.historySig === historySig && tv.messagesSig === messagesSig && tv.optSig === optSig && tv.tailSig === tailSig) {
        if (forceBottom) scrollViewportToBottom(vp);
        return;
      }
      const onlyTailChanged = tv.errSig === errSig && tv.historySig === historySig && tv.messagesSig === messagesSig && tv.optSig === optSig;
      if (onlyTailChanged) {
        const isTailOpen = Boolean(tv.tailDetails?.open);
        const atBottom = isTailOpen && (vp.scrollHeight - vp.scrollTop - vp.clientHeight < 64);
        patchLiveTail(tv, s, tailSrc.rawText, autoOpenTail, tailSig);
        if ((atBottom && tv.tailDetails?.open) || forceBottom) {
          vp.scrollTop = vp.scrollHeight;
        }
        return;
      }
      const prevHeight = vp.scrollHeight;
      const prevTop = vp.scrollTop;
      const atBottom = prevHeight - prevTop - vp.clientHeight < 64;
      const expandingReading = UI.loadingEarlierSid === s.session_id;
      if (tv.errSig !== errSig) {
        tv.errSig = errSig;
        const nodes = renderAlertNodes();
        if (nodes.length) tv.alertSlot.replaceChildren(...nodes);
        else tv.alertSlot.replaceChildren();
      }
      if (tv.historySig !== historySig) {
        tv.historySig = historySig;
        const hb = buildHistoryBar(s, tr);
        if (hb) tv.historySlot.replaceChildren(hb);
        else tv.historySlot.replaceChildren();
      }
      if (tv.messagesSig !== messagesSig) {
        tv.messagesSig = messagesSig;
        reconcileTimelineChunks(tv, s, tr);
      }
      if (tv.optSig !== optSig) {
        tv.optSig = optSig;
        const optCards = optSig ? renderOptimisticCards(s.session_id) : [];
        if (optCards.length) tv.optimisticSlot.replaceChildren(...optCards);
        else tv.optimisticSlot.replaceChildren();
      }
      if (tv.tailSig !== tailSig) {
        patchLiveTail(tv, s, tailSrc.rawText, autoOpenTail, tailSig);
      }
      vp.dataset.sig = fullSig;
      if (expandingReading) {
        UI.loadingEarlierSid = null;
        vp.scrollTop = Math.max(0, prevTop + (vp.scrollHeight - prevHeight));
      } else if (atBottom || forceBottom) {
        scrollViewportToBottom(vp);
      } else {
        vp.scrollTop = prevTop;
      }
      syncScrollLatestBtn(vp);
      return;
    }

    // Fresh mount of persistent TimelineView (session switch, raw->reading, or loading->ready).
    const nextTv = createTimelineView(s.session_id);
    nextTv.errSig = errSig;
    const initAlerts = renderAlertNodes();
    if (initAlerts.length) nextTv.alertSlot.replaceChildren(...initAlerts);
    nextTv.historySig = historySig;
    const hb = buildHistoryBar(s, tr);
    if (hb) nextTv.historySlot.replaceChildren(hb);
    nextTv.messagesSig = messagesSig;
    reconcileTimelineChunks(nextTv, s, tr);
    nextTv.optSig = optSig;
    const initOptCards = optSig ? renderOptimisticCards(s.session_id) : [];
    if (initOptCards.length) nextTv.optimisticSlot.replaceChildren(...initOptCards);
    patchLiveTail(nextTv, s, tailSrc.rawText, autoOpenTail, tailSig);
    vp._tv = nextTv;
    vp.dataset.sid = s.session_id;
    vp.dataset.mode = mode;
    vp.dataset.sig = fullSig;
    if (vp.className !== 'term-viewport') vp.className = 'term-viewport';
    vp.replaceChildren(nextTv.root);
    if (UI.loadingEarlierSid === s.session_id) UI.loadingEarlierSid = null;
    scrollViewportToBottom(vp);
    syncScrollLatestBtn(vp);
    return;
  }

  // Non-transcript path: compute cheap signature BEFORE building any DOM!
  const nextSig = computeNonTranscriptSig(s, mode, pane, curPvMode, pv, tr, glyphSig, baseErrSig, optSig, lang);
  if (sameView && vp.dataset.sig === nextSig) {
    if (forceBottom) scrollViewportToBottom(vp);
    return;
  }

  const prevHeight = sameView ? vp.scrollHeight : 0;
  const wasLoading = sameView && Boolean(vp.dataset.sig?.includes(':loading:'));
  const prevTop = sameView ? vp.scrollTop : 0;
  const atBottom = !sameView || wasLoading || forceBottom || (prevHeight - prevTop - vp.clientHeight < 64);
  const expandingRaw = sameView && pane && UI.loadingEarlierPane === pane && mode === 'raw';
  const built = buildOutputBody(s);
  vp._tv = null;
  vp.dataset.sid = s.session_id;
  vp.dataset.mode = mode;
  vp.dataset.sig = nextSig;
  const wantCls = `term-viewport${mode === 'raw' ? ' raw-mode' : ''}`;
  if (vp.className !== wantCls) vp.className = wantCls;
  vp.replaceChildren(built.node);
  if (expandingRaw) {
    UI.loadingEarlierPane = null;
    vp.scrollTop = Math.max(0, prevTop + (vp.scrollHeight - prevHeight));
  } else if (atBottom) {
    scrollViewportToBottom(vp);
  } else {
    vp.scrollTop = prevTop;
  }
  syncScrollLatestBtn(vp);
}

export function jumpToPrompt(userIdx) {
  if (UI.outputMode !== 'reading') { UI.outputMode = 'reading'; APP.rerender(); }
  UI.jumpLockUntil = Date.now() + 700;
  if (UI.wsShell?.rail) setOutlineActive(UI.wsShell.rail, userIdx);
  else {
    for (const it of document.querySelectorAll('.outline-item[data-turn-idx]')) {
      it.classList.toggle('active', it.dataset.turnIdx === String(userIdx));
    }
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
  APP.rerender();
}

function outlineRailPane(s, prompts) {
  if (UI.outputMode !== 'reading' || !prompts.length || UI.outlineCollapsed) return null;
  const fk = `pjump:${s.session_id}`;
  const listEl = h('div', { class: 'outline-list' });
  const itemByIdx = new Map();
  let railEl = null;
  const renderList = () => {
    const q = draft(fk).trim().toLowerCase();
    const filtered = q ? prompts.filter((p) => p.text.toLowerCase().includes(q) || `#${p.idx}`.includes(q)) : prompts;
    itemByIdx.clear();
    if (!filtered.length) {
      listEl.replaceChildren(h('div', { class: 'muted', style: 'padding:12px;font-size:12px;text-align:center' }, L('无匹配提问', 'No matching prompts')));
      if (railEl) railEl._activeIdx = '';
      return;
    }
    const frag = document.createDocumentFragment();
    const lastIdx = String(filtered[filtered.length - 1].idx);
    for (let i = 0; i < filtered.length; i++) {
      const p = filtered[i];
      const idxStr = String(p.idx);
      const itemEl = h('button', {
        type: 'button',
        class: `outline-item${i === filtered.length - 1 ? ' active' : ''}`,
        'data-turn-idx': idxStr,
        onclick: () => jumpToPrompt(p.idx),
      },
      h('div', { class: 'outline-item-top' },
        span('outline-item-idx', `#${p.idx}`),
        p.ts_ms ? span('tabular', clock(p.ts_ms)) : null),
      h('div', { class: 'outline-item-text' }, p.text));
      itemByIdx.set(idxStr, itemEl);
      frag.appendChild(itemEl);
    }
    listEl.replaceChildren(frag);
    if (railEl) railEl._activeIdx = lastIdx;
  };
  const filterInp = prompts.length > 6 ? (() => {
    const inp = field(fk, { class: 'inp sm', type: 'search', placeholder: L('过滤提问…', 'Filter prompts…'), autocomplete: 'off' });
    inp.addEventListener('input', renderList);
    return inp;
  })() : null;
  railEl = h('aside', { class: 'outline-rail', 'aria-label': L('提问目录', 'Prompt Outline'), 'data-cnt': String(prompts.length) },
    h('div', { class: 'outline-head' },
      h('div', { class: 'outline-head-row' },
        span('outline-title', L(`提问目录 · ${prompts.length}`, `Prompts · ${prompts.length}`)),
        btn('»', { cls: 'ghost xs', title: L('收起提问目录', 'Collapse outline'), onclick: () => setOutlineCollapsed(true) })),
      filterInp),
    listEl);
  railEl._itemByIdx = itemByIdx;
  railEl._activeIdx = '';
  renderList();
  return railEl;
}

function sidebarSessionItem(s, splitWindowKeys = null) {
  const curId = currentSession()?.session_id;
  const sel = curId === s.session_id;
  const subs = subagentsOf(s.session_id).filter((c) => UI.showEnded || c.state !== 'ended');
  const subCount = subs.length || s.subagent_count || 0;
  const subOpenKey = `subs:${s.session_id}`;
  const childSelected = subs.some((c) => c.session_id === curId);
  const subsOpen = childSelected || UI.open.has(subOpenKey);
  const pctVal = ctxPctOf(s.metrics);
  const tpsVal = tpsOf(s.metrics);
  const ttftVal = ttftOf(s.metrics);
  const tone = ctxTone(pctVal);
  const primaryTitle = s.tmux_label || s.tmux_target || base(s.git_root || s.cwd);
  const subMeta = s.tmux_label && s.tmux_target ? s.tmux_target : base(s.git_root || s.cwd);
  const meLabel = modelEffortLabel(s.model, s.effort);
  const paneBadge = paneDisplayId(s);
  const mux = muxKindOf(s);
  const closable = canCloseSessionWindow(s);
  const isSplit = Boolean(closable && splitWindowKeys && splitWindowKeys.has(windowGroupKeyOf(s)));
  const cardBtn = h('button', {
    type: 'button',
    class: `ws-item${sel ? ' sel' : ''}${s.state === 'suspended' ? ' is-suspended' : ''}`,
    'data-sid': s.session_id,
    onpointerenter: (e) => scheduleHoverPrefetch(s, e),
    onpointerleave: () => cancelHoverPrefetch(s.session_id),
    onclick: () => { cancelHoverPrefetch(); APP.openSession(s.session_id); },
  },
  h('div', { class: 'ws-item-top' },
    h('i', { class: `dot dot-${s.state}`, title: stateText(s.state) }),
    agentTag(s.agent),
    h('span', { class: 'ws-item-target grow', title: primaryTitle }, primaryTitle),
    paneBadge ? h('span', { class: 'tag host-pfx', title: paneBadge }, paneBadge) : null),
  h('div', { class: 'ws-item-sub' },
    h('span', { class: 'trunc grow' }, subMeta),
    s.state === 'suspended' ? span('tag soft-suspended', L('已休眠', 'Suspended')) : null,
    mux === 'zellij' ? span('tag mux-zj', 'zellij') : null,
    s.discovered_only && mux !== 'zellij' ? span('tag legacy', L('tmux 发现', 'tmux discovered')) : null,
    subCount ? span('tag tabular', L(`${subCount} 子会话`, `${subCount} subagents`)) : null),
  meLabel ? h('div', { class: 'ws-item-model', title: meLabel }, meLabel) : null,
  (pctVal != null || tpsVal != null || ttftVal != null) ? h('div', { class: 'ws-item-metrics' },
    pctVal != null ? h('span', { class: `row-nowrap${tone === 'high' ? ' ws-tel-high' : tone === 'warn' ? ' ws-tel-warn' : ''}`, style: 'gap:5px' },
      `CTX ${Math.round(pctVal)}%`, ctxBar(pctVal, true)) : null,
    tpsVal != null ? span('muted', `${tpsVal.toFixed(1)} tok/s`) : null,
    ttftVal != null ? h('span', {
      class: `ws-item-ttft${ttftTone(ttftVal) === 'fast' ? ' ttft-fast' : ttftTone(ttftVal) === 'slow' ? ' ttft-slow' : ' muted'}`,
      title: isFin(s.metrics?.avg_ttft_ms) && s.metrics.avg_ttft_ms > 0
        ? L(`最近首字 ${fmtMs(ttftVal)} · 平均 ${fmtMs(s.metrics.avg_ttft_ms)}`, `Latest TTFT ${fmtMs(ttftVal)} · Avg ${fmtMs(s.metrics.avg_ttft_ms)}`)
        : L(`最近首字延迟 ${fmtMs(ttftVal)}`, `Latest TTFT ${fmtMs(ttftVal)}`),
    }, L(`首字 ${fmtMs(ttftVal)}`, `TTFT ${fmtMs(ttftVal)}`)) : null) : null,
  s.last_message ? h('div', { class: 'ws-item-msg' }, s.last_message) : null);
  const closeTip = isSplit
    ? L(`关闭窗格 ${sessionLabel(s)}`, `Close pane ${sessionLabel(s)}`)
    : L(`关闭窗口 ${sessionLabel(s)}`, `Close window ${sessionLabel(s)}`);
  const card = h('div', { class: `ws-item-wrap${closable ? ' has-close' : ''}${sel ? ' is-sel' : ''}` },
    cardBtn,
    closable ? h('button', {
      type: 'button',
      class: 'ws-item-close',
      'data-close-sid': s.session_id,
      title: closeTip,
      'aria-label': closeTip,
      onclick: (e) => {
        e.stopPropagation();
        openCloseSessionModal(s.session_id);
      },
    }, closeCrossIcon()) : null);
  if (!subs.length) return card;
  const subTree = h('div', { class: 'ws-sub-wrap' },
    h('button', {
      type: 'button',
      class: 'ws-sub-toggle',
      onclick: () => {
        if (UI.open.has(subOpenKey)) UI.open.delete(subOpenKey);
        else UI.open.add(subOpenKey);
        APP.rerender();
      },
    }, L(`↳ ${subs.length} 个子会话 ${subsOpen ? '▾' : '▸'}`, `↳ ${subs.length} subagents ${subsOpen ? '▾' : '▸'}`)),
    subsOpen ? subs.map((c) => h('button', {
      type: 'button',
      class: `ws-sub-item${curId === c.session_id ? ' sel' : ''}`,
      onpointerenter: (e) => scheduleHoverPrefetch(c, e),
      onpointerleave: () => cancelHoverPrefetch(c.session_id),
      onclick: () => { cancelHoverPrefetch(); APP.openSession(c.session_id); },
    },
    h('i', { class: `dot dot-${c.state}`, title: stateText(c.state) }),
    h('span', { class: 'trunc grow' }, subagentDisplayName(c)),
    h('span', { class: 'dim', style: 'font-size:11px' }, stateText(c.state)))) : null);
  return h('div', { class: 'stack-xs', style: 'gap:2px' }, card, subTree);
}

export function createWorkspaceShell() {
  const sideCount = span('tag tabular', '0');
  const sideSearch = h('input', {
    class: 'inp sm',
    type: 'search',
    placeholder: L('搜索主机、会话、窗口或目录…', 'Search host, session, window, or path…'),
    autocomplete: 'off',
    'data-draft': 'ws:filter',
    value: draft('ws:filter'),
    oninput: (e) => { UI.drafts.set('ws:filter', e.target.value); APP.schedule(); },
  });
  const sideToggleSlot = h('span');
  const sideChips = h('div', { class: 'chips' });
  const sideShowEnded = h('input', {
    type: 'checkbox',
    checked: UI.showEnded,
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
      APP.schedule();
    },
  });
  const sideList = h('div', { class: 'ws-side-list' });
  sideList.addEventListener('pointerleave', () => cancelHoverPrefetch());
  const sideCloseBtn = h('button', {
    type: 'button',
    class: 'ws-side-close',
    'aria-label': L('关闭会话列表', 'Close sessions list'),
    title: L('关闭', 'Close'),
    onclick: () => { UI.mobileSessionsOpen = false; APP.rerender(); },
  }, '×');
  const sideBackdrop = h('div', {
    class: 'ws-side-backdrop',
    'aria-hidden': 'true',
    onclick: () => { UI.mobileSessionsOpen = false; APP.rerender(); },
  });
  const sidebar = h('aside', { class: 'ws-sidebar', 'aria-label': L('会话列表', 'Sessions') },
    h('div', { class: 'ws-side-head' },
      h('div', { class: 'ws-side-top' },
        h('div', { class: 'ws-side-title' }, L('会话列表', 'Sessions'), sideCount),
        h('div', { class: 'row-nowrap ws-side-top-acts', style: 'gap:6px' },
          btn(L('+ 新建交互会话', '+ New Session'), { cls: 'sm', title: L('在 tmux 中新开 claude / codex / agy 交互会话（快捷键 n）', 'Spawn interactive claude / codex / agy session in tmux (shortcut n)'), onclick: () => APP.openSpawnModal() }),
          sideCloseBtn)),
      h('div', { class: 'ws-search-row' }, sideSearch, sideToggleSlot),
      h('div', { class: 'row' },
        sideChips,
        h('label', { class: 'row muted', style: 'font-size:12px;margin-left:auto;cursor:pointer' }, sideShowEnded, L('含已结束', 'Show ended')))),
    sideList);

  const headEl = h('header', { class: 'ws-chat-head' });
  const crumbEl = h('div', { class: 'sub-crumb ws-hidden' });
  const reopenBtn = h('button', {
    type: 'button',
    class: 'outline-reopen-btn ws-hidden',
    title: L('展开提问目录', 'Open prompt outline'),
    onclick: () => setOutlineCollapsed(false),
  });
  const vp = h('div', { class: 'term-viewport' });
  let scrollRaf = 0;
  vp.addEventListener('scroll', () => {
    if (scrollRaf) return;
    scrollRaf = requestAnimationFrame(() => {
      scrollRaf = 0;
      syncScrollLatestBtn(vp);
    });
  }, { passive: true });
  const latestBtn = h('button', {
    type: 'button',
    class: 'scroll-latest-btn',
    onclick: () => {
      const s = currentSession();
      const isRaw = !s || s.agent === 'shell' || UI.outputMode === 'raw';
      const tr = s && !isRaw ? S.transcripts.get(s.session_id) : null;
      const pv = s && isRaw && s.pane ? S.previews.get(pvKeyOf(s.pane)) : null;
      const needsRefresh = Boolean(s && s.state !== 'ended' && (
        isRaw
          ? (!pv || !pv.updatedMs || (Date.now() - pv.updatedMs) > 3000)
          : (!tr || !tr.updatedMs || (Date.now() - tr.updatedMs) > 3000 || tr.autoPollPaused)
      ));
      if (needsRefresh) {
        vp.scrollTop = vp.scrollHeight;
        syncScrollLatestBtn(vp);
        loadSessionOutput(s, { silent: !tr?.autoPollPaused });
      } else {
        vp.scrollTo({ top: vp.scrollHeight, behavior: 'smooth' });
      }
    },
  }, L('↓ 回到最新', '↓ Latest'));
  const chatStage = h('div', { class: 'chat-stage' }, vp, latestBtn);
  const slashEl = h('div', { class: 'slash-menu ws-hidden' });
  const modeBtns = [['direct', L('直接发送', 'Direct')], ['feature', L('新功能协作', 'Feature Pipeline')], ['debug', L('两速查错', 'Two-Speed Debug')]].map(([k, l]) => h('button', {
    type: 'button',
    role: 'tab',
    class: 'mode-btn',
    'data-mode': k,
    onclick: () => { UI.composerMode = k; closeSlashMenu(); APP.rerender(); },
  }, l));
  const modeHint = h('span', { class: 'dim mode-hint', style: 'font-size:12px' });
  const modeBar = h('div', { class: 'mode-bar' },
    h('div', { class: 'mode-seg', role: 'tablist', 'aria-label': L('输入模式', 'Composer mode') }, modeBtns),
    modeHint);
  const attachBar = h('div', { class: 'attach-bar ws-hidden' });
  const deliveryAlertEl = h('div', { class: 'ws-hidden' });
  const ta = h('textarea', {
    id: 'composer-input',
    rows: '2',
    class: 'composer-ta',
    placeholder: L('发消息给当前会话…（输入 / 唤出命令，Cmd+Enter 发送）', 'Message this session… (/ for commands, Cmd+Enter to send)'),
  });
  const renderDeliveryAlert = (sess) => {
    const sid = sess?.session_id || comp.sid;
    const s = sess || (sid ? S.sessions.get(sid) : null);
    const issue = sid ? composerDeliveryIssues.get(sid) : null;
    const lang = getLang();
    const busy = sid ? UI.busy.has(`send:${sid}`) : false;
    const visible = Boolean(issue && UI.composerMode === 'direct' && s && !s.is_subagent && s.state !== 'ended');
    const sig = visible
      ? `${sid}|${issue.status}|${issue.ts}|${issue.reason}|${s.pane || ''}|${busy ? 1 : 0}|${lang}`
      : '';
    if (deliveryAlertEl.dataset.sig === sig && deliveryAlertEl.classList.contains('ws-hidden') === !visible) return;
    deliveryAlertEl.dataset.sig = sig;
    if (!visible) {
      deliveryAlertEl.classList.add('ws-hidden');
      deliveryAlertEl.replaceChildren();
      return;
    }
    const isErr = issue.status === 'failed' || issue.status === 'error';
    const title = issue.status === 'unconfirmed'
      ? L('投递未确认 · 草稿已保留', 'Delivery unconfirmed · Draft preserved')
      : (issue.status === 'queued'
        ? L('未能直接注入终端 · 草稿已保留', 'Not injected into terminal · Draft preserved')
        : L('消息未送达 · 草稿已保留', 'Message not delivered · Draft preserved'));
    deliveryAlertEl.classList.remove('ws-hidden');
    deliveryAlertEl.replaceChildren(
      h('div', {
        class: `callout ${isErr ? 'err' : 'warn'}`,
        'data-delivery-issue': issue.status,
        style: 'display:flex;align-items:center;justify-content:space-between;gap:12px;flex-wrap:wrap;padding:8px 12px;margin:0',
      },
      h('div', { class: 'stack-xs grow', style: 'min-width:0' },
        h('strong', { style: 'font-size:12.5px' }, title),
        h('span', { style: 'font-size:12px;word-break:break-word' }, issue.reason)),
      h('div', { class: 'row-nowrap', style: 'gap:6px;flex-shrink:0' },
        btn(L('重试发送', 'Retry'), { cls: 'sm', busyKey: `send:${sid}`, onclick: () => sendDirect() }),
        s.pane ? btn(L('查看终端', 'Inspect terminal'), {
          cls: 'ghost xs',
          onclick: () => {
            UI.outputMode = 'raw';
            loadPreview(s.pane, { mode: pvModeOf(s.pane), silent: false });
            APP.rerender();
          },
        }) : null,
        btn(L('关闭', 'Dismiss'), {
          cls: 'ghost xs',
          onclick: () => {
            composerDeliveryIssues.delete(sid);
            renderDeliveryAlert(s);
          },
        }))),
    );
  };
  const sendDirect = () => {
    const sid = comp.sid;
    const s = sid ? S.sessions.get(sid) : null;
    if (!s) return;
    if (UI.busy.has(`send:${sid}`)) return;
    const dk = `ws:msg:${sid}`;
    const rawBefore = ta.value || '';
    const text = rawBefore.trim();
    const attsSnapshot = [...(UI.attachments.get(sid) || [])];
    const sentAttIds = new Set(attsSnapshot.map((a) => a.id));
    if (attsSnapshot.some((a) => a.status === 'uploading')) { toast(L('图片仍在上传中，请稍候再发送', 'Image still uploading, please wait'), 'warn'); return; }
    if (!text && !attsSnapshot.length) { toast(L('请输入要发送的内容、图片或粘贴片段', 'Enter a message, image, or pasted snippet'), 'warn'); return; }
    closeSlashMenu();

    const useOptimistic = s.agent !== 'shell' && UI.outputMode === 'reading' && UI.composerMode === 'direct';
    let optNonce = null;
    let optEntry = null;
    if (useOptimistic) {
      const curTr = S.transcripts.get(sid);
      const anchorTotal = (curTr?.state === 'ok' && curTr.source === 'transcript')
        ? Math.max(curTr.totalMessages || 0, (Number.isFinite(curTr.startSeq) ? curTr.startSeq : 0) + (curTr.messages?.length || 0))
        : 0;
      optNonce = `opt-${Date.now().toString(36)}-${Math.random().toString(36).slice(2, 7)}`;
      optEntry = addOptimisticTurn(sid, {
        nonce: optNonce,
        text,
        created_ms: Date.now(),
        anchor_total: anchorTotal,
        status: 'sending',
        rawBefore,
        attachments: attsSnapshot,
        _expireTimer: 0,
      });
      composerDeliveryIssues.delete(sid);
      renderDeliveryAlert(s);
      // Clear composer & sent attachments immediately in the same frame; rawBefore + attsSnapshot stay safe in memory.
      UI.drafts.delete(dk);
      ta.value = '';
      ta.style.height = 'auto';
      autoGrow(ta);
      syncTaMetrics();
      comp.lastSel = { sid, start: 0, end: 0 };
      const curAtts = UI.attachments.get(sid) || [];
      const keptAtts = curAtts.filter((a) => !sentAttIds.has(a.id));
      if (keptAtts.length) UI.attachments.set(sid, keptAtts);
      else UI.attachments.delete(sid);
      renderAttachBar(sid);
      UI.forceScrollBottomSid = sid;
      notifyOutputUpdated(sid);
    } else {
      UI.drafts.set(dk, rawBefore);
    }

    const rollbackOptimistic = () => {
      if (!useOptimistic || !optNonce) return;
      const removed = removeOptimisticTurn(sid, optNonce);
      if (removed) {
        if (comp.sid === sid) {
          if (!ta.value.trim()) {
            UI.drafts.set(dk, rawBefore);
            ta.value = rawBefore;
            ta.style.height = 'auto';
            autoGrow(ta);
            syncTaMetrics();
            comp.lastSel = { sid, start: rawBefore.length, end: rawBefore.length };
          }
        } else if (!draft(dk).trim()) {
          UI.drafts.set(dk, rawBefore);
        }
        if (attsSnapshot.length) {
          const curAtts = UI.attachments.get(sid) || [];
          const existingIds = new Set(curAtts.map((a) => a.id));
          const restored = [...attsSnapshot.filter((a) => !existingIds.has(a.id)), ...curAtts];
          UI.attachments.set(sid, restored);
        }
        if (comp.sid === sid) renderAttachBar(sid);
      }
      notifyOutputUpdated(sid);
    };

    act(`send:${sid}`, async () => {
      const payload = { target: { type: 'session', id: sid }, text, interactive: true };
      if (attsSnapshot.length) {
        payload.attachments = attsSnapshot.map((a) => (a.kind === 'image'
          ? { kind: 'image', title: a.title, path: a.path, ...(a.token ? { token: a.token } : {}) }
          : { kind: 'text', title: a.title, content: a.content, ...(a.token ? { token: a.token } : {}) }));
      }
      let r;
      try {
        r = await api('POST', '/api/dispatch', payload);
      } catch (e) {
        // Preserve draft and attachments on network/HTTP error and surface 1-click Retry.
        rollbackOptimistic();
        if (!useOptimistic && !UI.drafts.has(dk) && rawBefore) UI.drafts.set(dk, rawBefore);
        composerDeliveryIssues.set(sid, {
          status: 'error',
          reason: errText(e),
          report: null,
          ts: Date.now(),
        });
        renderDeliveryAlert(S.sessions.get(sid) || s);
        throw e;
      }

      const delivered = Boolean(r && (r.status === 'delivered' || r.status === 'sent'));
      if (!delivered) {
        // NEVER silently wipe composer draft or attachments when delivery failed, queued, or unconfirmed.
        rollbackOptimistic();
        if (!useOptimistic && !UI.drafts.has(dk) && rawBefore) UI.drafts.set(dk, rawBefore);
        composerDeliveryIssues.set(sid, {
          status: r?.status || 'failed',
          reason: describeDeliveryIssue(r),
          report: r,
          ts: Date.now(),
        });
        renderDeliveryAlert(S.sessions.get(sid) || s);
        if (comp.sid === sid) ta.focus({ preventScroll: true });
        return r;
      }

      composerDeliveryIssues.delete(sid);
      if (useOptimistic && optNonce) {
        updateOptimisticTurnStatus(sid, optNonce, 'sent');
        if (optEntry && hasOptimisticTurn(sid, optNonce)) {
          optEntry._expireTimer = setTimeout(() => {
            if (removeOptimisticTurn(sid, optNonce)) notifyOutputUpdated(sid);
          }, OPTIMISTIC_EXPIRE_MS);
        }
        reconcileOptimisticTurns(sid, S.transcripts.get(sid));
        notifyOutputUpdated(sid);
      } else {
        const curAtts = UI.attachments.get(sid) || [];
        const keptAtts = curAtts.filter((a) => !sentAttIds.has(a.id));
        if (keptAtts.length) UI.attachments.set(sid, keptAtts);
        else UI.attachments.delete(sid);

        if (comp.sid === sid) {
          // Only clear the textarea if the user has not already started typing a new message in-flight.
          if (ta.value === rawBefore) {
            UI.drafts.delete(dk);
            ta.value = '';
            ta.style.height = 'auto';
            autoGrow(ta);
            syncTaMetrics();
            comp.lastSel = { sid, start: 0, end: 0 };
          } else {
            UI.drafts.set(dk, ta.value);
          }
          renderAttachBar(sid);
        } else if (draft(dk) === rawBefore) {
          UI.drafts.delete(dk);
        }
      }
      renderDeliveryAlert(S.sessions.get(sid) || s);
      if (s.pane) {
        const hid = hostOf(s);
        const hRec = isRemoteHost(hid) ? S.hosts.get(hid) : null;
        const rttShift = isRemoteHost(hid) && typeof hRec?.rtt_ms === 'number' && Number.isFinite(hRec.rtt_ms)
          ? Math.max(0, Math.min(300, Math.round(hRec.rtt_ms)))
          : 0;
        if (useOptimistic && optNonce && S.conn === 'live') {
          const checkIfPending = () => {
            if (hasOptimisticTurn(sid, optNonce)) {
              const latest = S.sessions.get(sid) || s;
              loadSessionOutput(latest, { silent: true });
            }
          };
          setTimeout(checkIfPending, 900 + rttShift);
          setTimeout(checkIfPending, 2600 + rttShift);
        } else {
          setTimeout(() => loadSessionOutput(S.sessions.get(sid) || s, { silent: true }), 450 + rttShift);
          setTimeout(() => loadSessionOutput(S.sessions.get(sid) || s, { silent: true }), 1500 + rttShift);
          setTimeout(() => loadSessionOutput(S.sessions.get(sid) || s, { silent: true }), 3000 + rttShift);
        }
      }
      return r;
    }, (r) => {
      if (r && (r.status === 'delivered' || r.status === 'sent')) {
        return { msg: L(`已发送：${deliveryText(r)}`, `Sent: ${deliveryText(r)}`), kind: 'ok' };
      }
      const reason = describeDeliveryIssue(r);
      if (r?.status === 'unconfirmed') {
        return { msg: L(`投递未确认（草稿已保留）：${reason}`, `Delivery unconfirmed (draft preserved): ${reason}`), kind: 'warn' };
      }
      if (r?.status === 'queued') {
        return { msg: L(`未能直接注入终端（草稿已保留）：${reason}`, `Not injected into terminal (draft preserved): ${reason}`), kind: 'warn' };
      }
      return { msg: L(`发送失败（草稿已保留）：${reason}`, `Send failed (draft preserved): ${reason}`), kind: 'err' };
    });
  };
  const syncTaMetrics = () => {
    const minH = window.innerWidth <= 768 ? 34 : 52;
    lastTaLen = ta.value.length;
    lastTaHeight = parseInt(ta.style.height, 10) || minH;
  };
  let lastTaLen = 0;
  let lastTaHeight = 52;
  const fastAutoGrow = () => {
    const isMob = window.innerWidth <= 768;
    const minH = isMob ? 34 : 52;
    const maxH = isMob ? 132 : 220;
    const val = ta.value;
    const len = val.length;
    const shrank = len < lastTaLen;
    lastTaLen = len;
    if (!shrank) {
      if (lastTaHeight >= maxH) return;
      const sh = ta.scrollHeight;
      if (sh <= ta.clientHeight) return;
      const nextH = Math.min(maxH, Math.max(minH, sh));
      if (nextH !== lastTaHeight) {
        ta.style.height = `${nextH}px`;
        lastTaHeight = nextH;
      }
      return;
    }
    if (lastTaHeight <= minH) return;
    autoGrow(ta);
    lastTaHeight = parseInt(ta.style.height, 10) || minH;
  };
  ta.addEventListener('input', () => {
    if (comp.sid) UI.drafts.set(`ws:msg:${comp.sid}`, ta.value);
    saveComposerSelection(ta);
    fastAutoGrow();
    if (comp.agent !== 'shell' && (UI.slash.open || /^\s*\//.test(ta.value))) {
      updateSlashFromInput(ta, comp.agent);
    }
  });
  ta.addEventListener('click', () => {
    saveComposerSelection(ta);
    if (comp.agent !== 'shell' && (UI.slash.open || /^\s*\//.test(ta.value))) {
      updateSlashFromInput(ta, comp.agent);
    }
  });
  ta.addEventListener('keyup', (e) => {
    saveComposerSelection(ta);
    if (comp.agent !== 'shell' && ['ArrowLeft', 'ArrowRight', 'Home', 'End'].includes(e.key) && (UI.slash.open || /^\s*\//.test(ta.value))) {
      updateSlashFromInput(ta, comp.agent);
    }
  });
  ta.addEventListener('mouseup', () => saveComposerSelection(ta));
  ta.addEventListener('select', () => saveComposerSelection(ta));
  ta.addEventListener('blur', () => saveComposerSelection(ta));
  ta.addEventListener('paste', (e) => {
    const cd = e.clipboardData;
    const range = [ta.selectionStart ?? ta.value.length, ta.selectionEnd ?? ta.value.length];
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
        uploadImageFiles(comp.sid, imgFiles, { range, focus: true });
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
      const item = attachFoldedPaste(sid, txt, { range, focus: true });
      if (item) toast(L(`已折叠为 ${item.token}（${item.lines} 行 · ${fmtBytes(item.bytes)}），发送时将按原位置展开`, `Folded into ${item.token} (${item.lines} lines · ${fmtBytes(item.bytes)}); will expand in place on send`), 'ok');
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
    if (e.key === 'Enter' && (e.metaKey || e.ctrlKey) && !e.isComposing && e.keyCode !== 229) { e.preventDefault(); sendDirect(); }
  });
  const imgInput = h('input', {
    type: 'file',
    accept: 'image/*',
    multiple: true,
    class: 'ws-hidden',
    'aria-label': L('选择图片上传', 'Choose image to upload'),
    onchange: () => {
      if (imgInput.files?.length) uploadImageFiles(comp.sid, imgInput.files);
      imgInput.value = '';
    },
  });
  const slashBtn = h('button', {
    type: 'button',
    class: 'btn ghost sm comp-slash-btn',
    title: L('快捷命令（输入 /）', 'Quick commands (insert /)'),
    'aria-label': L('快捷命令', 'Quick commands'),
    onclick: () => {
      if (UI.slash.open) {
        closeSlashMenu();
        return;
      }
      if (comp.agent === 'shell') return;
      const val = ta.value || '';
      if (!/^\s*\//.test(val)) {
        const next = val ? `/${val}` : '/';
        ta.value = next;
        if (comp.sid) UI.drafts.set(`ws:msg:${comp.sid}`, next);
        autoGrow(ta);
        syncTaMetrics();
        ta.focus({ preventScroll: true });
        ta.setSelectionRange(1, 1);
        saveComposerSelection(ta);
      } else {
        const slashPos = val.indexOf('/') + 1;
        ta.focus({ preventScroll: true });
        ta.setSelectionRange(slashPos, slashPos);
        saveComposerSelection(ta);
      }
      updateSlashFromInput(ta, comp.agent);
    },
  }, '/');
  const imgBtn = h('button', {
    type: 'button',
    class: 'btn ghost sm comp-img-btn',
    title: L('上传图片或截图（支持粘贴与拖拽）', 'Upload image or screenshot (supports paste & drag-drop)'),
    onclick: () => imgInput.click(),
  }, L('图片', 'Image'));
  const sendBtn = h('button', { type: 'button', class: 'btn primary sm comp-send-btn', onclick: sendDirect }, L('发送', 'Send'));
  const directRow = h('div', { class: 'composer-direct-row' }, ta, h('div', { class: 'composer-actions' }, imgInput, slashBtn, imgBtn, sendBtn));
  const pipeSlot = h('div', { class: 'stack-sm ws-hidden' });
  const readonlyHint = h('div', { class: 'comp-readonly ws-hidden' });
  const suspendBanner = h('div', { class: 'comp-suspend-banner ws-hidden' });
  const card = h('div', { class: 'composer-card' }, slashEl, readonlyHint, suspendBanner, modeBar, deliveryAlertEl, attachBar, directRow, pipeSlot);
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
  const promptBarSlot = h('div', { class: 'prompt-dock-wrap ws-hidden' });
  const dock = h('div', { class: 'composer-dock' }, card);
  const comp = { dock, card, slashEl, readonlyHint, suspendBanner, modeBar, modeBtns, modeHint, deliveryAlertEl, renderDeliveryAlert, attachBar, directRow, ta, slashBtn, imgBtn, sendBtn, pipeSlot, sid: null, agent: null, lastSel: null, syncTaMetrics };
  const convCol = h('div', { class: 'ws-conv-col' }, reopenBtn, chatStage, promptBarSlot, dock);
  const bodySplit = h('div', { class: 'ws-body-split' }, convCol);
  const emptyMain = emptyState(L('选择一个会话开始对话', 'Select a Session to Start'), L('从左侧点击任意正在运行的 tmux / Zellij 会话，即可查看实时终端并直接发送指令。', 'Pick any running tmux or Zellij session on the left to inspect output and send messages.'),
    h('div', { class: 'row', style: 'justify-content:center;margin-top:4px' },
      btn(L('打开会话列表', 'Browse Sessions'), { cls: 'ws-empty-side-btn', onclick: () => { UI.mobileSessionsOpen = true; APP.rerender(); } }),
      btn(L('新建交互会话', 'New Session'), { cls: 'primary', onclick: () => APP.openSpawnModal() })));
  emptyMain.classList.add('ws-hidden');
  const main = h('section', { class: 'ws-main' }, headEl, crumbEl, bodySplit, emptyMain);
  const sideResizer = h('div', {
    class: 'ws-side-resizer',
    role: 'separator',
    tabindex: '0',
    'aria-orientation': 'vertical',
    'aria-label': L('拖动调整左侧会话栏宽度（双击恢复默认）', 'Drag to resize sessions sidebar (double-click to reset)'),
    title: L('拖动调整宽度 · 双击恢复默认', 'Drag to resize · Double-click to reset'),
  });
  const grid = h('div', { class: 'ws-grid' }, sideBackdrop, sidebar, sideResizer, main);

  let savedW = SIDEBAR_DEFAULT_W;
  try {
    const raw = localStorage.getItem(SIDEBAR_WIDTH_KEY);
    if (raw != null && raw.trim() !== '') {
      const parsed = Number(raw);
      if (Number.isFinite(parsed) && parsed > 0) savedW = parsed;
    }
  } catch { /* ignore */ }
  let currentW = clampSidebarWidth(savedW);
  const applySidebarWidth = (w, { persist = false } = {}) => {
    const maxW = sidebarMaxWidth();
    const clamped = clampSidebarWidth(w);
    currentW = clamped;
    grid.style.setProperty('--ws-side-w', `${clamped}px`);
    sideResizer.setAttribute('aria-valuemin', String(SIDEBAR_MIN_W));
    sideResizer.setAttribute('aria-valuemax', String(maxW));
    sideResizer.setAttribute('aria-valuenow', String(clamped));
    if (persist) {
      savedW = clamped;
      try {
        if (clamped === SIDEBAR_DEFAULT_W) localStorage.removeItem(SIDEBAR_WIDTH_KEY);
        else localStorage.setItem(SIDEBAR_WIDTH_KEY, String(clamped));
      } catch { /* ignore */ }
    }
    return clamped;
  };
  applySidebarWidth(savedW, { persist: false });

  let dragCleanup = null;
  sideResizer.addEventListener('pointerdown', (e) => {
    if (e.button !== 0 || ((typeof window !== 'undefined' && window.innerWidth) || 1440) <= 960) return;
    if (dragCleanup) dragCleanup(false);
    e.preventDefault();
    const startX = e.clientX;
    const startW = currentW;
    const pointerId = e.pointerId;
    let active = true;
    grid.classList.add('is-resizing');
    document.body.classList.add('is-col-resizing');
    try { sideResizer.setPointerCapture(pointerId); } catch { /* ignore */ }
    const onMove = (ev) => {
      if (!active) return;
      applySidebarWidth(startW + (ev.clientX - startX), { persist: false });
    };
    const cleanup = (persist = true) => {
      if (!active) return;
      active = false;
      dragCleanup = null;
      window.removeEventListener('pointermove', onMove);
      window.removeEventListener('pointerup', onEnd);
      window.removeEventListener('pointercancel', onEnd);
      grid.classList.remove('is-resizing');
      document.body.classList.remove('is-col-resizing');
      try {
        if (sideResizer.hasPointerCapture?.(pointerId)) sideResizer.releasePointerCapture(pointerId);
      } catch { /* ignore */ }
      if (persist) applySidebarWidth(currentW, { persist: true });
    };
    const onEnd = () => cleanup(true);
    dragCleanup = cleanup;
    window.addEventListener('pointermove', onMove);
    window.addEventListener('pointerup', onEnd);
    window.addEventListener('pointercancel', onEnd);
  });
  sideResizer.addEventListener('dblclick', (e) => {
    if (((typeof window !== 'undefined' && window.innerWidth) || 1440) <= 960) return;
    e.preventDefault();
    applySidebarWidth(SIDEBAR_DEFAULT_W, { persist: true });
  });
  sideResizer.addEventListener('keydown', (e) => {
    if (((typeof window !== 'undefined' && window.innerWidth) || 1440) <= 960) return;
    const step = e.shiftKey ? 40 : 16;
    if (e.key === 'ArrowLeft') {
      e.preventDefault();
      applySidebarWidth(currentW - step, { persist: true });
    } else if (e.key === 'ArrowRight') {
      e.preventDefault();
      applySidebarWidth(currentW + step, { persist: true });
    } else if (e.key === 'Home' || e.key === 'Enter') {
      e.preventDefault();
      applySidebarWidth(SIDEBAR_DEFAULT_W, { persist: true });
    }
  });
  if (typeof window !== 'undefined' && window.addEventListener) {
    window.addEventListener('resize', () => {
      if (!grid.isConnected) return;
      applySidebarWidth(savedW, { persist: false });
    }, { passive: true });
  }

  const errSlot = h('div', { class: 'ws-hidden', style: 'padding:10px 16px 0' });
  return {
    errSlot,
    grid,
    sideResizer,
    syncSidebarWidth: () => applySidebarWidth(savedW, { persist: false }),
    sideCount,
    sideSearch,
    sideToggleSlot,
    sideChips,
    sideShowEnded,
    sideList,
    headEl,
    mobOutlineBtn: null,
    crumbEl,
    bodySplit,
    convCol,
    reopenBtn,
    vp,
    latestBtn,
    promptBarSlot,
    emptyMain,
    comp,
    rail: null,
    railSig: '',
    promptBarSig: '',
    errBannerSig: '',
    sideSig: '',
    toggleSig: '',
    chipsSig: '',
    headSig: '',
    telSig: '',
    crumbSig: '',
    pipeSig: '',
    wasMobileSideOpen: false,
  };
}

export function mountOrPatchWorkspace(el) {
  if (!UI.wsShell) UI.wsShell = createWorkspaceShell();
  const W = UI.wsShell;
  if (W.grid.parentNode !== el) {
    el.replaceChildren(W.errSlot, W.grid);
    W.syncSidebarWidth?.();
  }
  const errBannerSig = S.loadError ? `${S.loadError}:${getLang()}` : '';
  if (W.errBannerSig !== errBannerSig) {
    W.errBannerSig = errBannerSig;
    if (S.loadError) {
      W.errSlot.classList.remove('ws-hidden');
      W.errSlot.replaceChildren(errorBanner(L(`状态同步异常：${S.loadError}`, `State sync error: ${S.loadError}`), L('重试同步', 'Retry sync'), loadAll));
    } else {
      W.errSlot.classList.add('ws-hidden');
      W.errSlot.replaceChildren();
    }
  }
  patchWorkspace(W);
}

export function patchWorkspace(W) {
  const roots = rootSessions().filter((s) => UI.showEnded || s.state !== 'ended');
  const q = draft('ws:filter').trim().toLowerCase();
  const hayOf = (s) => [
    hostOf(s), hostDisplayName(hostOf(s), s), s.tmux_label, s.tmux_target,
    s.pane, s.agent, s.model, s.git_root, s.cwd, s.last_message, s.session_id,
  ].join(' ').toLowerCase();
  const match = (s) => (!UI.agentFilter || s.agent === UI.agentFilter)
    && (!q || hayOf(s).includes(q) || subagentsOf(s.session_id).some((c) => hayOf(c).includes(q)));
  const shown = roots.filter(match);

  // Build Host -> Mux Session Group hierarchy.
  const hostOrder = [];
  const hostMap = new Map();
  const ensureHostBucket = (hid) => {
    if (!hostMap.has(hid)) {
      hostMap.set(hid, { id: hid, groups: new Map(), total: 0 });
      hostOrder.push(hid);
    }
    return hostMap.get(hid);
  };
  // Include all known hosts (so degraded/empty remote hosts stay visible & renameable).
  for (const hRec of S.hosts.values()) {
    if (!q || hRec.id.toLowerCase().includes(q) || (hRec.display_name || '').toLowerCase().includes(q)) {
      if (S.hosts.size > 1 || hRec.id === 'local') ensureHostBucket(hRec.id);
    }
  }
  for (const s of shown) {
    const hid = hostOf(s);
    const hb = ensureHostBucket(hid);
    const g = tmuxGroupOf(s);
    if (!hb.groups.has(g)) hb.groups.set(g, []);
    hb.groups.get(g).push(s);
    hb.total++;
  }
  hostOrder.sort((a, b) => {
    if (a === 'local') return -1;
    if (b === 'local') return 1;
    return a.localeCompare(b);
  });

  const active = currentSession();
  const activeRootId = active?.is_subagent && active.parent_session_id ? active.parent_session_id : active?.session_id;
  const allCollapseKeys = [];
  for (const hid of hostOrder) {
    const hb = hostMap.get(hid);
    for (const [g, list] of hb.groups.entries()) {
      const cKey = hid === 'local' ? g : `${hid}:${g}`;
      allCollapseKeys.push({ cKey, g, list });
    }
  }
  if (!UI.groupsInited && allCollapseKeys.length >= 3) {
    UI.groupsInited = true;
    for (const { cKey, g, list } of allCollapseKeys) {
      if (g === NON_TMUX_GROUP) { UI.collapsedGroups.add(cKey); continue; }
      const hasSel = list.some((s) => s.session_id === activeRootId);
      const hasBusy = list.some((s) => ['working', 'needs_permission', 'needs_input', 'error'].includes(s.state));
      if (!hasSel && !hasBusy) UI.collapsedGroups.add(cKey);
    }
    saveCollapsedGroups();
  }
  const isSideOpen = Boolean(UI.mobileSessionsOpen);
  if (isSideOpen && !W.wasMobileSideOpen) {
    prefetchTopMobileSessions(shown);
  }
  W.wasMobileSideOpen = isSideOpen;
  W.grid.classList.toggle('is-side-open', isSideOpen);

  const allGroupsCollapsed = allCollapseKeys.length > 0 && allCollapseKeys.every(({ cKey }) => UI.collapsedGroups.has(cKey));
  const allHostsCollapsed = hostOrder.length > 0 && hostOrder.every((hid) => UI.collapsedHosts.has(hid));
  const allCollapsed = allGroupsCollapsed || allHostsCollapsed;
  const toggleAllGroups = () => {
    if (allCollapsed) {
      UI.collapsedGroups.clear();
      UI.collapsedHosts.clear();
      saveCollapsedHosts();
      try { localStorage.setItem(NON_TMUX_OPEN_KEY, '1'); } catch { /* ignore */ }
    } else {
      allCollapseKeys.forEach(({ cKey }) => UI.collapsedGroups.add(cKey));
      try { localStorage.setItem(NON_TMUX_OPEN_KEY, '0'); } catch { /* ignore */ }
    }
    saveCollapsedGroups();
    APP.rerender();
  };

  const lang = getLang();
  const shownCountStr = String(shown.length);
  if (W.sideCount.textContent !== shownCountStr) W.sideCount.textContent = shownCountStr;
  if (document.activeElement !== W.sideSearch && W.sideSearch.value !== draft('ws:filter')) W.sideSearch.value = draft('ws:filter');
  if (W.sideShowEnded.checked !== UI.showEnded) W.sideShowEnded.checked = UI.showEnded;

  const showToggleAll = allCollapseKeys.length > 1 || UI.collapsedHosts.size > 0;
  const toggleSig = `${showToggleAll ? 1 : 0}:${allCollapsed ? 1 : 0}:${lang}`;
  if (W.toggleSig !== toggleSig) {
    W.toggleSig = toggleSig;
    W.sideToggleSlot.replaceChildren(showToggleAll ? btn(allCollapsed ? L('全部展开', 'Expand all') : L('全部收起', 'Collapse all'), { cls: 'ghost xs', onclick: toggleAllGroups }) : '');
  }

  const filterAgents = (roots.some((s) => s.agent === 'shell') || UI.agentFilter === 'shell') ? [...AGENTS, 'shell'] : AGENTS;
  const chipsSig = `${filterAgents.join(',')}|${UI.agentFilter}|${lang}`;
  if (W.chipsSig !== chipsSig) {
    W.chipsSig = chipsSig;
    W.sideChips.replaceChildren(...['', ...filterAgents].map((a) => h('button', {
      type: 'button',
      class: `chip${UI.agentFilter === a ? ' on' : ''}`,
      onclick: () => { UI.agentFilter = a; APP.schedule(); },
    }, a || L('全部', 'All'))));
  }

  const visibleHostBlocks = hostOrder
    .map((hid) => hostMap.get(hid))
    .filter((hb) => hb.total > 0 || (S.hosts.size > 1 && S.hosts.has(hb.id)));

  // Precompute active multi-pane tmux windows in O(N) for split-pane tooltips & signature tracking.
  const windowPaneSets = new Map();
  for (const sess of S.sessions.values()) {
    if (!canCloseSessionWindow(sess)) continue;
    const wk = windowGroupKeyOf(sess);
    if (!wk) continue;
    let set = windowPaneSets.get(wk);
    if (!set) { set = new Set(); windowPaneSets.set(wk, set); }
    set.add(String(sess.pane).trim());
  }
  const splitWindowKeys = new Set();
  for (const [wk, set] of windowPaneSets.entries()) {
    if (set.size > 1) splitWindowKeys.add(wk);
  }

  // Signature-guard entire sidebar list so SSE bursts, tail ticks, and typing do ZERO sidebar rebuilds.
  const curId = active?.session_id || '';
  const sideSigParts = [lang, curId, activeRootId || '', q, UI.agentFilter, UI.showEnded ? 1 : 0, roots.length, S.hosts.size];
  for (let hi = 0; hi < visibleHostBlocks.length; hi++) {
    const hb = visibleHostBlocks[hi];
    const hRec = S.hosts.get(hb.id);
    const retryPaused = Boolean(hRec?.retry_paused);
    const retryCd = hb.id !== 'local' && !retryPaused && (hRec?.status === 'degraded' || hRec?.status === 'offline')
      ? formatRetryCountdown(hRec?.next_retry_ms)
      : '';
    const hostCollapsed = !q && UI.collapsedHosts.has(hb.id);
    sideSigParts.push(`H:${hb.id}:${hostCollapsed ? 1 : 0}:${hRec?.display_name || ''}:${hRec?.status || ''}:${hRec?.rtt_ms ?? ''}:${hRec?.sync_ms ?? ''}:${retryPaused ? 1 : 0}:${retryCd}:${UI.busy.has(`probe-host:${hb.id}`) ? 1 : 0}:${UI.busy.has(`pause-host:${hb.id}`) ? 1 : 0}:${hRec?.error || ''}:${hb.total}`);
    for (const [g, list] of hb.groups.entries()) {
      const cKey = hb.id === 'local' ? g : `${hb.id}:${g}`;
      const isCollapsed = !q && UI.collapsedGroups.has(cKey);
      sideSigParts.push(`G:${cKey}:${isCollapsed ? 1 : 0}:${list.length}`);
      for (let si = 0; si < list.length; si++) {
        const x = list[si];
        const subs = subagentsOf(x.session_id).filter((c) => UI.showEnded || c.state !== 'ended');
        const subOpen = UI.open.has(`subs:${x.session_id}`) || subs.some((c) => c.session_id === curId);
        const subSig = subs.length ? `${subOpen ? 1 : 0}:${subs.map((c) => `${c.session_id}:${c.state}:${c.tmux_label || ''}`).join(',')}` : '';
        const isSplit = splitWindowKeys.has(windowGroupKeyOf(x)) ? 1 : 0;
        sideSigParts.push(`S:${x.session_id}:${x.state}:${x.agent}:${x.tmux_label || ''}:${x.tmux_target || ''}:${x.pane || ''}:${x.model || ''}:${x.effort || ''}:${x.mux_kind || ''}:${x.discovered_only ? 1 : 0}:${x.subagent_count || 0}:${x.metrics?.context_used_pct ?? ''}:${x.metrics?.tps ?? ''}:${x.metrics?.ttft_ms ?? ''}:${x.metrics?.avg_ttft_ms ?? ''}:${hashStr(x.last_message || '')}:${isSplit}:${subSig}`);
      }
    }
  }
  const sideSig = hashStr(sideSigParts.join('|'));

  if (W.sideSig !== sideSig) {
    W.sideSig = sideSig;
    const prevSideTop = W.sideList.scrollTop;
    if (!shown.length && S.hosts.size <= 1) {
      W.sideList.replaceChildren(emptyState(
        roots.length ? L('没有匹配的会话', 'No Matching Sessions') : L('暂未发现活跃会话', 'No Active Sessions Found'),
        roots.length ? L('尝试清空搜索关键词或切换 Agent 过滤条件。', 'Try clearing the search query or Agent filter.') : L('启动中的 tmux / Zellij 会话会自动同步到这里，也可直接新建一个交互会话。', 'Active tmux / Zellij sessions sync here automatically, or click below to create one.'),
        !roots.length ? btn(L('新建交互会话', 'New Session'), { cls: 'primary sm', onclick: () => APP.openSpawnModal() }) : null,
      ));
    } else {
      W.sideList.replaceChildren(...visibleHostBlocks.map((hb) => {
        const hRec = S.hosts.get(hb.id);
        const hStatus = hRec?.status || 'online';
        const hKind = hRec?.kind || (hb.id === 'local' ? 'local' : 'peer');
        const hName = hostDisplayName(hb.id, [...hb.groups.values()][0]?.[0]);
        const sortedGroups = [...hb.groups.entries()].sort((a, b) => {
          if (a[0] === NON_TMUX_GROUP) return 1;
          if (b[0] === NON_TMUX_GROUP) return -1;
          const pa = Math.min(...a[1].map((s) => STATE_PRIO[s.state] ?? 5));
          const pb = Math.min(...b[1].map((s) => STATE_PRIO[s.state] ?? 5));
          return pa - pb || a[0].localeCompare(b[0]);
        });
        const isRemote = hb.id !== 'local';
        const hostCollapsed = !q && UI.collapsedHosts.has(hb.id);
        const rttBadge = isRemote && hStatus !== 'offline' ? buildRttPill(hRec?.rtt_ms, hRec?.sync_ms) : null;
        const hasSync = isRemote && hStatus !== 'offline' && typeof hRec?.sync_ms === 'number' && Number.isFinite(hRec.sync_ms) && hRec.sync_ms >= 0;
        const hostDotTip = [
          hName,
          hostStatusLabel(hStatus),
          isRemote && hStatus !== 'offline' ? fmtRtt(hRec?.rtt_ms) : '',
          hasSync ? L(`同步 ${fmtRtt(hRec.sync_ms)}`, `Sync ${fmtRtt(hRec.sync_ms)}`) : '',
          hRec?.active_endpoint ? `(${hRec.active_endpoint})` : '',
        ].filter(Boolean).join(' · ');
        const toggleHostTip = hostCollapsed
          ? L(`展开主机 ${hName}`, `Expand host ${hName}`)
          : L(`收起主机 ${hName}`, `Collapse host ${hName}`);
        const hostHead = h('div', { class: 'ws-host-head' },
          h('button', {
            type: 'button',
            class: 'ws-host-toggle',
            'data-host-toggle': hb.id,
            'aria-expanded': String(!hostCollapsed),
            'aria-label': toggleHostTip,
            title: toggleHostTip,
            onclick: () => {
              if (UI.collapsedHosts.has(hb.id)) UI.collapsedHosts.delete(hb.id);
              else UI.collapsedHosts.add(hb.id);
              saveCollapsedHosts();
              APP.rerender();
            },
          },
          span('ws-host-caret', hostCollapsed ? '▸' : '▾'),
          h('i', { class: `dot dot-${hStatus}`, title: hostDotTip }),
          h('span', { class: 'ws-host-name', title: `${hName} (${hb.id})` }, hName),
          span('ws-host-id', hb.id),
          hKind !== 'local' ? span('tag', hKind === 'ssh' ? 'SSH' : 'Peer') : null,
          rttBadge),
          h('div', { class: 'row-nowrap ws-host-acts', style: 'gap:4px;flex-shrink:0' },
            h('button', {
              type: 'button',
              class: 'ws-host-rename',
              'data-host': hb.id,
              title: L(`重命名主机 ${hName}（${hb.id}）`, `Rename host ${hName} (${hb.id})`),
              'aria-label': L(`重命名主机 ${hb.id}`, `Rename host ${hb.id}`),
              onclick: () => APP.openRenameHostModal(hb.id),
            }, '✎'),
            span('tag tabular', hb.total)));
        const retryPaused = Boolean(hRec?.retry_paused);
        const retryCd = isRemote && !retryPaused && (hStatus === 'degraded' || hStatus === 'offline')
          ? formatRetryCountdown(hRec?.next_retry_ms)
          : '';
        const hostStatusBanner = isRemote && (hStatus === 'degraded' || hStatus === 'offline')
          ? h('div', { class: `ws-host-banner is-${hStatus}${retryPaused ? ' is-paused' : ''}` },
            h('div', { class: 'ws-host-banner-top' },
              span(`tag ${hStatus === 'offline' ? 'soft-red' : 'soft-amber'}`, hostStatusLabel(hStatus)),
              retryPaused
                ? span('ws-host-retry-cd ws-host-paused-txt', L('已停止重试', 'Auto-retry paused'))
                : (retryCd ? span('ws-host-retry-cd tabular', retryCd) : null),
              h('div', { class: 'ws-host-banner-acts' },
                !retryPaused ? btn(L('停止重试', 'Stop retry'), {
                  cls: 'xs ghost ws-host-pause-btn',
                  busyKey: `pause-host:${hb.id}`,
                  title: L(`停止自动重试主机 ${hName}`, `Pause automatic retries for host ${hName}`),
                  onclick: () => pauseHostRetryApi(hb.id),
                }) : null,
                btn(L('重连', 'Retry'), {
                  cls: 'xs ws-host-probe-btn',
                  busyKey: `probe-host:${hb.id}`,
                  title: retryPaused
                    ? L(`恢复并重新连接主机 ${hName}`, `Resume and reconnect host ${hName}`)
                    : L(`立即重新探测主机 ${hName}`, `Probe and reconnect host ${hName} now`),
                  onclick: () => probeHostApi(hb.id),
                }))),
            hRec?.error ? h('div', { class: 'ws-host-err', title: hRec.error }, hRec.error) : null)
          : null;

        const groupNodes = hostCollapsed ? [] : sortedGroups.map(([g, list]) => {
          const cKey = hb.id === 'local' ? g : `${hb.id}:${g}`;
          const isCollapsed = !q && UI.collapsedGroups.has(cKey);
          const hasSel = list.some((s) => s.session_id === activeRootId);
          const attnSess = list.find((s) => ['needs_permission', 'needs_input', 'error'].includes(s.state))
            || list.find((s) => s.state === 'working');
          const groupCls = `ws-group${isCollapsed ? ' is-collapsed' : ' is-open'}${hasSel ? ' has-sel' : ''}`;
          return h('div', { class: groupCls },
            h('button', {
              type: 'button',
              class: 'ws-group-btn',
              'aria-expanded': String(!isCollapsed),
              onclick: () => {
                const willExpand = UI.collapsedGroups.has(cKey);
                if (willExpand) UI.collapsedGroups.delete(cKey);
                else UI.collapsedGroups.add(cKey);
                if (g === NON_TMUX_GROUP) { try { localStorage.setItem(NON_TMUX_OPEN_KEY, willExpand ? '1' : '0'); } catch { /* ignore */ } }
                saveCollapsedGroups();
                APP.rerender();
              },
            },
            h('span', { class: 'row-nowrap', style: 'gap:5px;min-width:0' },
              span('ws-group-caret', isCollapsed ? '▸' : '▾'),
              h('span', { class: 'ws-group-name trunc' }, groupDisplayName(g)),
              isCollapsed && attnSess ? h('i', { class: `dot dot-${attnSess.state}`, title: stateText(attnSess.state) }) : null),
            span('ws-group-cnt tabular', list.length)),
            !isCollapsed ? h('div', { class: 'ws-group-body' }, list.map((s) => sidebarSessionItem(s, splitWindowKeys))) : null);
        });
        return h('div', { class: `ws-host-block${hostCollapsed ? ' is-collapsed' : ''}`, 'data-host-id': hb.id },
          hostHead,
          hostStatusBanner,
          hostCollapsed
            ? null
            : (groupNodes.length ? groupNodes : h('div', { class: 'dim ws-empty-host', style: 'font-size:12px;padding:2px 8px' }, L('暂无活跃会话', 'No active sessions'))));
      }));
    }
    W.sideList.scrollTop = prevSideTop;
  }

  if (!active) {
    W.headEl.classList.add('ws-hidden');
    W.crumbEl.classList.add('ws-hidden');
    W.bodySplit.classList.add('ws-hidden');
    W.emptyMain.classList.remove('ws-hidden');
    syncInteractivePromptBar(W, null);
    return;
  }
  W.headEl.classList.remove('ws-hidden');
  W.bodySplit.classList.remove('ws-hidden');
  W.emptyMain.classList.add('ws-hidden');
  const s = active;
  const dir = s.git_root || s.cwd || '';
  const meLabel = modelEffortLabel(s.model, s.effort);
  const paneBadge = paneDisplayId(s);
  const titleTip = [hostDisplayName(hostOf(s), s), dir, s.tmux_target, paneBadge, meLabel].filter(Boolean).join(' · ');
  const parent = s.is_subagent && s.parent_session_id ? S.sessions.get(s.parent_session_id) : null;
  const displayTitle = s.is_subagent ? subagentDisplayName(s) : sessionLabel(s);
  const isShell = s.agent === 'shell';
  const effectiveMode = isShell ? 'raw' : UI.outputMode;
  const headHostId = hostOf(s);
  const headIsRemote = isRemoteHost(headHostId);
  const headHostLabel = headIsRemote ? hostDisplayName(headHostId, s) : '';
  const canSusp = canSuspendSession(s);
  const canWake = canResumeSession(s);
  const suspBusy = UI.busy.has(`suspend:${s.session_id}`);
  const wakeBusy = UI.busy.has(`resume:${s.session_id}`);
  const headBaseSig = `${s.session_id}|${displayTitle}|${paneBadge}|${s.agent}|${muxKindOf(s)}|${meLabel}|${s.state}|${effectiveMode}|${headHostLabel}|${titleTip}|${canSusp ? 1 : 0}|${canWake ? 1 : 0}|${suspBusy ? 1 : 0}|${wakeBusy ? 1 : 0}|${lang}`;
  const nextTelSig = telemetrySigOf(s);
  if (W.headSig !== headBaseSig) {
    W.headSig = headBaseSig;
    W.telSig = nextTelSig;
    const viewToggle = isShell
      ? h('div', { class: 'view-seg', role: 'tablist', 'aria-label': L('显示模式', 'Display mode') },
        h('button', { type: 'button', role: 'tab', class: 'on', onclick: () => { if (s.pane) loadPreview(s.pane, { mode: pvModeOf(s.pane), silent: false }); } }, L('终端', 'Terminal')))
      : h('div', { class: 'view-seg', role: 'tablist', 'aria-label': L('显示模式', 'Display mode') },
        h('button', { type: 'button', role: 'tab', class: UI.outputMode === 'reading' ? 'on' : '', onclick: () => { UI.outputMode = 'reading'; APP.rerender(); } }, L('阅读', 'Reading')),
        h('button', { type: 'button', role: 'tab', class: UI.outputMode === 'raw' ? 'on' : '', onclick: () => { UI.outputMode = 'raw'; if (s.pane) loadPreview(s.pane, { mode: pvModeOf(s.pane), silent: true }); APP.rerender(); } }, L('终端', 'Terminal')));
    const mobMenuBtn = h('button', {
      type: 'button',
      class: 'ws-mob-menu-btn',
      'aria-label': L('打开会话列表', 'Open sessions list'),
      title: L('打开会话列表', 'Open sessions list'),
      onclick: () => { UI.mobileSessionsOpen = !UI.mobileSessionsOpen; APP.rerender(); },
    }, menuDrawerIcon());
    const mobPctVal = ctxPctOf(s.metrics);
    const mobTitleBtn = h('button', {
      type: 'button',
      class: 'ws-head-mob-title',
      'aria-label': L(`切换会话：${displayTitle}`, `Switch session: ${displayTitle}`),
      title: titleTip,
      onclick: () => { UI.mobileSessionsOpen = true; APP.rerender(); },
    },
    h('span', { class: 'ws-mob-title-row' },
      h('i', { class: `dot dot-${s.state}` }),
      h('span', { class: 'ws-mob-title-name trunc' }, displayTitle),
      span('ws-mob-caret', '▾')),
    h('span', { class: 'ws-mob-sub-row' },
      span(`ws-mob-agent a-${s.agent || 'x'}`, s.agent || '?'),
      buildMobCtxEl(s),
      headIsRemote ? h('span', { class: 'ws-mob-host trunc' }, headHostLabel) : null,
      mobPctVal == null && !headIsRemote ? h('span', { class: 'trunc' }, stateText(s.state)) : null));
    const mobOutlineBtn = h('button', {
      type: 'button',
      class: 'btn sm ws-outline-mob-btn ws-hidden',
      'aria-label': L('打开提问目录', 'Open prompt outline'),
      title: L('提问目录', 'Prompt outline'),
      onclick: () => APP.openDrawer('outline', { sessionId: s.session_id }),
    }, outlineListIcon(), span('ws-outline-cnt tabular', '0'));
    W.mobOutlineBtn = mobOutlineBtn;
    const mobModeBtn = isShell
      ? h('button', {
        type: 'button',
        class: 'btn sm on ws-mob-mode-btn',
        'aria-label': L('刷新终端视图', 'Refresh terminal view'),
        title: L('刷新终端', 'Refresh terminal'),
        onclick: () => { if (s.pane) loadPreview(s.pane, { mode: pvModeOf(s.pane), silent: false }); },
      }, modeSwitchIcon(true), span(null, L('终端', 'Term')))
      : h('button', {
        type: 'button',
        class: `btn sm ws-mob-mode-btn${effectiveMode === 'raw' ? ' on' : ''}`,
        'aria-label': effectiveMode === 'reading' ? L('切换到原始终端视图', 'Switch to raw terminal view') : L('切换回阅读视图', 'Switch back to reading view'),
        title: effectiveMode === 'reading' ? L('切换到原始终端', 'Switch to raw terminal') : L('切回阅读视图', 'Switch to reading view'),
        onclick: () => {
          if (UI.outputMode === 'reading') {
            UI.outputMode = 'raw';
            if (s.pane) loadPreview(s.pane, { mode: pvModeOf(s.pane), silent: true });
          } else {
            UI.outputMode = 'reading';
          }
          APP.rerender();
        },
      }, modeSwitchIcon(effectiveMode === 'reading'), span(null, effectiveMode === 'reading' ? L('终端', 'Term') : L('阅读', 'Read')));
    const suspendWakeBtn = canWake
      ? btn(L('唤醒', 'Wake'), {
        cls: 'sm primary ws-head-wake-btn',
        busyKey: `resume:${s.session_id}`,
        title: L('原地唤醒并恢复原会话上下文', 'Wake and restore this conversation in-place'),
        onclick: () => resumeSessionApi(s.session_id),
      })
      : (canSusp
        ? btn(L('休眠', 'Suspend'), {
          cls: 'ghost sm ws-head-susp-btn',
          busyKey: `suspend:${s.session_id}`,
          title: L('结束当前 Agent 进程释放 CPU/内存，保留终端窗口以便随时恢复原会话', 'Exit agent process to save CPU/RAM while keeping pane resumable'),
          onclick: () => suspendSessionApi(s.session_id),
        })
        : null);
    const detailsBtn = h('button', {
      type: 'button',
      class: 'btn sm ws-head-info-btn',
      title: L('展开右侧检查抽屉（切到终端 / 刷新 / 性能与上下文 / 元数据）', 'Open inspector drawer (terminal / refresh / metrics / metadata)'),
      onclick: () => APP.openDrawer('session', { sessionId: s.session_id }),
    }, span('ws-lbl-desk', L('详情', 'Details')), span('ws-lbl-mob', L('详情', 'Info')));
    const deskPane = paneBadge ? span('tag host-pfx ws-head-desk-item', paneBadge) : null;
    const deskAgent = agentTag(s.agent);
    deskAgent.classList.add('ws-head-desk-item');
    const deskMux = muxKindOf(s) === 'zellij' ? span('tag mux-zj ws-head-desk-item', 'zellij') : null;
    const deskModel = meLabel ? span('tag mono ws-model-pill ws-head-desk-item', meLabel) : null;
    const deskStatus = statusInd(s.state, stateText(s.state));
    deskStatus.classList.add('ws-head-desk-item');
    W.headEl.replaceChildren(
      h('div', { class: 'ws-head-left', title: titleTip },
        mobMenuBtn,
        mobTitleBtn,
        h('h2', { class: 'ws-head-title' }, displayTitle),
        deskPane,
        deskAgent,
        deskMux,
        deskModel,
        deskStatus),
      h('div', { class: 'ws-head-right' },
        buildTelemetryStrip(s),
        mobOutlineBtn,
        viewToggle,
        mobModeBtn,
        suspendWakeBtn,
        detailsBtn),
    );
  } else if (W.telSig !== nextTelSig) {
    patchTelemetryStrip(s.session_id);
  }

  const crumbSig = s.is_subagent ? `${s.session_id}|${s.parent_session_id || ''}|${parent ? sessionLabel(parent) : ''}|${lang}` : '';
  if (W.crumbSig !== crumbSig) {
    W.crumbSig = crumbSig;
    if (s.is_subagent) {
      W.crumbEl.classList.remove('ws-hidden');
      W.crumbEl.replaceChildren(
        h('span', { class: 'row' }, span('tag', L('子会话', 'Subagent')), h('span', null, L(`所属 ${parent ? sessionLabel(parent) : s.parent_session_id}`, `Parent: ${parent ? sessionLabel(parent) : s.parent_session_id}`))),
        s.parent_session_id ? btn(L('← 返回主会话', '← Back to parent'), { cls: 'ghost xs', onclick: () => APP.openSession(s.parent_session_id) }) : '',
      );
    } else {
      W.crumbEl.classList.add('ws-hidden');
      W.crumbEl.replaceChildren();
    }
  }

  if (!isShell && !S.transcripts.has(s.session_id)) loadTranscript(s.session_id, { silent: false });
  if (s.pane && !S.previews.has(pvKeyOf(s.pane))) loadPreview(s.pane, { mode: pvModeOf(s.pane), silent: false });
  if (!isShell) loadSlashCommands(s.agent);

  syncOutputViewport(W.vp, s);
  syncOutlineRail(W, s, effectiveMode);
  syncInteractivePromptBar(W, s);

  const C = W.comp;
  if (s.state === 'suspended') {
    if (C.sid && !C.directRow.classList.contains('ws-hidden')) {
      UI.drafts.set(`ws:msg:${C.sid}`, C.ta.value);
    }
    C.sid = null;
    C.agent = null;
    closeSlashMenu();
    C.readonlyHint.classList.add('ws-hidden');
    C.modeBar.classList.add('ws-hidden');
    C.deliveryAlertEl.classList.add('ws-hidden');
    C.attachBar.classList.add('ws-hidden');
    C.directRow.classList.add('ws-hidden');
    C.pipeSlot.classList.add('ws-hidden');
    C.suspendBanner.classList.remove('ws-hidden');
    const wakeInFlight = UI.busy.has(`resume:${s.session_id}`);
    const hasResume = Boolean(s.resume_id);
    const suspSig = `${s.session_id}|${s.resume_id || ''}|${wakeInFlight ? 1 : 0}|${lang}`;
    if (C.suspendBanner.dataset.sig !== suspSig) {
      C.suspendBanner.dataset.sig = suspSig;
      C.suspendBanner.replaceChildren(
        h('div', { class: 'comp-suspend-info' },
          h('div', { class: 'comp-suspend-title' },
            L('会话已休眠 · 资源已释放', 'Session suspended · resources reclaimed'),
            hasResume
              ? h('code', { class: 'comp-suspend-rid' }, short(s.resume_id, 14))
              : span('tag', L('重开空白会话', 'Fresh on wake'))),
          h('div', { class: 'comp-suspend-sub' },
            hasResume
              ? L(
                'Agent 进程已安全退出以节省 CPU/内存，终端窗口与对话记录已保留；点击右侧按钮或在终端按 Enter 即可原地恢复原会话。',
                'Agent process exited to save CPU/RAM while keeping your pane and transcript intact. Click Wake or press Enter in the terminal to resume in-place.',
              )
              : L(
                'Agent 进程已退出以节省 CPU/内存，终端窗口已保留；点击右侧按钮或在终端按 Enter 将在该目录启动全新交互会话。',
                'Agent process exited to save CPU/RAM while keeping your pane open. Click Wake or press Enter in the terminal to launch a fresh interactive session.',
              ))),
        btn(hasResume ? L('唤醒原会话', 'Wake Session') : L('唤醒会话', 'Relaunch Session'), {
          cls: 'sm primary comp-wake-btn',
          busyKey: `resume:${s.session_id}`,
          onclick: () => resumeSessionApi(s.session_id),
        }),
      );
    }
    return;
  }
  C.suspendBanner.classList.add('ws-hidden');
  C.suspendBanner.dataset.sig = '';
  C.modeBar.classList.remove('ws-hidden');

  const readOnlyMsg = s.is_subagent
    ? L('只读子会话 · 无法直接在此发送消息，请返回主会话操作', 'Read-only subagent · Cannot dispatch directly here; return to parent session')
    : (s.state === 'ended' ? L('该会话已结束 · 请切换至活跃会话发送指令', 'Session has ended · Switch to an active session to send messages') : '');
  if (readOnlyMsg) {
    C.readonlyHint.textContent = readOnlyMsg;
    C.readonlyHint.classList.remove('ws-hidden');
  } else {
    C.readonlyHint.classList.add('ws-hidden');
  }
  const compHostId = hostOf(s);
  const compIsRemote = isRemoteHost(compHostId);
  const compHostRec = compIsRemote ? S.hosts.get(compHostId) : null;
  const sending = UI.busy.has(`send:${s.session_id}`);
  const effCompMode = isShell ? 'direct' : UI.composerMode;
  for (const b of C.modeBtns) {
    b.classList.toggle('ws-hidden', isShell && b.dataset.mode !== 'direct');
    b.classList.toggle('on', b.dataset.mode === effCompMode);
    b.disabled = !!readOnlyMsg;
  }
  const isOfflineDirect = compIsRemote && compHostRec?.status === 'offline' && effCompMode === 'direct';
  if (sending && effCompMode === 'direct') {
    const rttPart = compIsRemote && typeof compHostRec?.rtt_ms === 'number' && Number.isFinite(compHostRec.rtt_ms)
      ? ` · ${fmtRtt(compHostRec.rtt_ms)}`
      : '';
    const proto = compHostRec?.kind === 'peer' ? 'Peer' : 'SSH';
    C.modeHint.textContent = compIsRemote
      ? L(`正在通过 ${proto} 发送至 ${hostDisplayName(compHostId, s)}${rttPart}…`, `Sending over ${proto} to ${hostDisplayName(compHostId, s)}${rttPart}…`)
      : L('正在发送至终端…', 'Sending to terminal…');
    C.modeHint.classList.add('is-sending-hint', 'is-critical');
  } else {
    C.modeHint.classList.remove('is-sending-hint');
    C.modeHint.classList.toggle('is-critical', Boolean(readOnlyMsg || isOfflineDirect));
    C.modeHint.textContent = readOnlyMsg || (isOfflineDirect
      ? L(`主机 ${hostDisplayName(compHostId, s)} 已离线 · 发送将尝试重连`, `Host ${hostDisplayName(compHostId, s)} is offline · send will attempt reconnect`)
      : (isShell
        ? L('发送终端命令或输入 · Cmd+Enter 执行', 'Send terminal command or input · Cmd+Enter to execute')
        : (effCompMode === 'direct' ? L('输入 / 唤出命令 · Cmd+Enter 发送', '/ for commands · Cmd+Enter to send') : L('可选模板 · 留空使用默认配置', 'Optional routing · Leave blank for defaults'))));
  }
  if (effCompMode === 'direct' || readOnlyMsg) {
    C.directRow.classList.remove('ws-hidden');
    C.pipeSlot.classList.add('ws-hidden');
    const isMob = window.innerWidth <= 768;
    const wantRows = isMob ? 1 : 2;
    if (C.ta.rows !== wantRows) C.ta.rows = wantRows;
    if (C.sid !== s.session_id) {
      if (C.sid) UI.drafts.set(`ws:msg:${C.sid}`, C.ta.value);
      C.sid = s.session_id;
      C.agent = s.agent;
      C.ta.dataset.draft = `ws:msg:${s.session_id}`;
      C.ta.value = draft(`ws:msg:${s.session_id}`);
      C.lastSel = { sid: s.session_id, start: C.ta.value.length, end: C.ta.value.length };
      C.ta.style.height = 'auto';
      autoGrow(C.ta);
      C.syncTaMetrics?.();
      closeSlashMenu();
    } else {
      C.agent = s.agent;
      if (isShell) closeSlashMenu();
    }
    const ph = readOnlyMsg || (isMob
      ? (isShell
        ? L(`输入命令发送到 ${sessionLabel(s)}…`, `Send command to ${sessionLabel(s)}…`)
        : L(`发消息给 ${sessionLabel(s)}…`, `Message ${sessionLabel(s)}…`))
      : (isShell
        ? L(`输入终端命令发送到 ${sessionLabel(s)}…（Cmd+Enter 执行）`, `Send terminal command to ${sessionLabel(s)}… (Cmd+Enter to execute)`)
        : L(`发消息给 ${sessionLabel(s)}…（输入 / 唤出命令，Cmd+Enter 发送）`, `Message ${sessionLabel(s)}… (/ for commands, Cmd+Enter to send)`)));
    if (C.ta.placeholder !== ph) C.ta.placeholder = ph;
    const wantSendLabel = sending ? L('发送中…', 'Sending…') : L('发送', 'Send');
    if (C.sendBtn.textContent !== wantSendLabel) C.sendBtn.textContent = wantSendLabel;
    C.ta.disabled = !!readOnlyMsg;
    C.slashBtn.classList.toggle('ws-hidden', isShell);
    C.slashBtn.disabled = !!readOnlyMsg;
    C.imgBtn.disabled = !!readOnlyMsg;
    C.sendBtn.classList.remove('is-busy');
    C.sendBtn.classList.toggle('is-sending', sending);
    C.sendBtn.disabled = sending || !!readOnlyMsg;
    C.renderDeliveryAlert(s);
    renderAttachBar(s.session_id);
  } else {
    C.directRow.classList.add('ws-hidden');
    C.deliveryAlertEl.classList.add('ws-hidden');
    C.attachBar.classList.add('ws-hidden');
    C.pipeSlot.classList.remove('ws-hidden');
    closeSlashMenu();
    C.pipeSlot.replaceChildren(workspacePipelineComposer(s, UI.composerMode));
  }
}
