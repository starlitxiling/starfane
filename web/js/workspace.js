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
  errorBanner,
  field,
  fmtBytes,
  h,
  skeleton,
  span,
  statusInd,
  toast,
  tpsOf,
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
  qs,
  sessionNeedsLiveTail,
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
import {
  APP,
  S,
  UI,
  base,
  currentSession,
  draft,
  enc,
  hashStr,
  hostDisplayName,
  hostOf,
  modelEffortLabel,
  muxKindOf,
  paneDisplayId,
  pickDefaultSession,
  pvKeyOf,
  pvModeOf,
  rootSessions,
  saveCollapsedGroups,
  sessionLabel,
  stateText,
  subagentDisplayName,
  subagentsOf,
  tmuxGroupOf,
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

export function updateOutputClock(sessionId) {
  if (UI.view !== 'workspace' || currentSession()?.session_id !== sessionId) return;
  const el = $('.term-updated');
  if (el) el.textContent = `更新于 ${clock(Date.now())}`;
}

export function patchTelemetryStrip(sessionId) {
  if (UI.view !== 'workspace' || currentSession()?.session_id !== sessionId) return;
  const s = S.sessions.get(sessionId);
  const el = $('.ws-telemetry');
  if (el && s && el.dataset.telSid === sessionId) {
    el.replaceWith(buildTelemetryStrip(s));
  }
}

export function syncOutlineSpy(vp) {
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

export function syncScrollLatestBtn(vp) {
  if (!vp) return;
  const b = $('.scroll-latest-btn');
  if (b) {
    const dist = vp.scrollHeight - vp.scrollTop - vp.clientHeight;
    b.classList.toggle('show', dist > 120);
  }
  syncOutlineSpy(vp);
}

export function collectUserPrompts(s) {
  if (!s || s.agent === 'shell') return [];
  const tr = S.transcripts.get(s.session_id);
  let msgs = [];
  if (tr?.state === 'ok' && tr.source === 'transcript' && tr.messages.length > 0) {
    msgs = tr.messages;
  } else if (s.pane) {
    const pv = S.previews.get(pvKeyOf(s.pane));
    if (pv?.text?.trim()) msgs = parseTerminalTurns(pv.text, s.agent);
  }
  const out = [];
  let uIdx = 0;
  for (const m of msgs) {
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
    out.push({ idx: uIdx, text: preview || '（空消息）', ts_ms: m.ts_ms || null });
  }
  return out;
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

export function notifyOutputUpdated(sessionId) {
  if (UI.view !== 'workspace' || currentSession()?.session_id !== sessionId) { APP.schedule(); return; }
  const vp = $('.term-viewport');
  const s = currentSession();
  const effMode = s?.agent === 'shell' ? 'raw' : UI.outputMode;
  if (!vp || !s || vp.dataset.sid !== s.session_id || vp.dataset.mode !== effMode) {
    APP.schedule();
    return;
  }
  const built = buildOutputBody(s);
  if (vp.dataset.sig === built.sig) {
    updateOutputClock(sessionId);
    return;
  }
  const prevHeight = vp.scrollHeight;
  const wasLoading = vp.dataset.sig?.includes(':loading:');
  const atBottom = wasLoading || (prevHeight - vp.scrollTop - vp.clientHeight < 64);
  const prevTop = vp.scrollTop;
  const expandingReading = UI.loadingEarlierSid === sessionId && effMode === 'reading';
  const expandingRaw = s.pane && UI.loadingEarlierPane === s.pane && effMode === 'raw';
  vp.dataset.sig = built.sig;
  vp.className = `term-viewport${effMode === 'raw' ? ' raw-mode' : ''}`;
  vp.replaceChildren(built.node);
  if (expandingReading || expandingRaw) {
    if (expandingReading) UI.loadingEarlierSid = null;
    if (expandingRaw) UI.loadingEarlierPane = null;
    vp.scrollTop = Math.max(0, prevTop + (vp.scrollHeight - prevHeight));
  } else if (atBottom) {
    scrollViewportToBottom(vp);
  } else {
    vp.scrollTop = prevTop;
  }
  syncScrollLatestBtn(vp);
  updateOutputClock(sessionId);
  const pCnt = collectUserPrompts(s).length;
  const rail = $('.outline-rail') || $('.outline-reopen-btn');
  const prevCnt = rail ? Number(rail.dataset.cnt || 0) : 0;
  if (pCnt !== prevCnt) APP.schedule();
}

export function ackSessionError(s) {
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

function buildGlyphTogglePill() {
  const native = detectNerdFontSupport();
  const eff = effectiveGlyphMode();
  const label = native && UI.termGlyphMode === 'auto'
    ? 'Nerd Font ✓'
    : (eff === 'compat' ? '图标: 兼容' : '图标: 原始');
  const cls = `glyph-pill${native && eff === 'raw' ? ' native' : eff === 'compat' ? ' on' : ''}`;
  const title = eff === 'compat'
    ? '当前自动将 Nerd Font / Powerline 私有图标转为通用 Unicode（避免乱码方块）；点击切换原始字符'
    : (native ? '已检测到本地 Nerd Font，正在原生渲染图标；点击可切到通用兼容图标' : '正在显示终端原始字符（若出现方块乱码，点击切回兼容图标模式）');
  return h('button', { type: 'button', class: cls, title, onclick: cycleGlyphMode }, label);
}

function buildRawToolbar(pane, curMode, actualLines, pv) {
  const key = pvKeyOf(pane, curMode);
  const isLoading = S.previews.get(key)?.state === 'loading';
  const totalLines = pv?.totalLines || actualLines;
  const statusText = curMode === 'all'
    ? `显示全部 ${actualLines} 行${pv?.truncated ? '（已达 4 MiB 安全上限）' : ''}`
    : (totalLines > actualLines ? `显示 ${actualLines} / 共 ${totalLines} 行` : `显示 ${actualLines} 行`);
  return h('div', { class: 'raw-toolbar' },
    h('div', { class: 'row-nowrap', style: 'gap:8px;min-width:0' },
      span('muted tabular', statusText),
      pv?.paginated ? span('tag soft-blue', '已暂停自动滚动') : null),
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
      }, step === 'all' ? '全部历史' : `${step} 行`))),
      buildGlyphTogglePill(),
      btn('刷新', { cls: 'xs', onclick: () => loadPreview(pane, { mode: curMode, silent: false }) })));
}

function buildRawOlderBar(pane, pv) {
  if (!pv || pv.state !== 'ok' || !pv.hasMore || !(pv.nextBeforeLine > 0)) return null;
  const remaining = pv.nextBeforeLine;
  return h('div', { class: 'history-bar' },
    span('muted tabular', `上方还有 ${remaining} 行更早的终端滚动历史`),
    h('div', { class: 'row-nowrap', style: 'gap:6px' },
      btn(`加载更早 (+${Math.min(PREVIEW_PAGE_STEP, remaining)} 行)`, {
        cls: 'xs',
        busyKey: `pvmore:${pane}`,
        onclick: () => loadEarlierPreview(pane, PREVIEW_PAGE_STEP),
      }),
      btn('加载全部历史', {
        cls: 'ghost xs',
        onclick: () => {
          UI.pvLines.set(pane, 'all');
          loadPreview(pane, { mode: 'all', silent: false });
          APP.schedule();
        },
      })));
}

function buildHistoryBar(s, tr) {
  if (!tr || !tr.hasMore) return null;
  const shown = tr.messages?.length || 0;
  const total = tr.totalMessages || shown;
  const earlier = tr.nextBefore ?? tr.startSeq ?? Math.max(0, total - shown);
  return h('div', { class: 'history-bar' },
    span('muted tabular', `已显示最近 ${shown} / 共 ${total} 条对话（上方还有 ${earlier} 条）`),
    h('div', { class: 'row-nowrap', style: 'gap:6px' },
      btn(`加载更早 (+${Math.min(TRANSCRIPT_PAGE_STEP, earlier)})`, {
        cls: 'xs',
        busyKey: `trmore:${s.session_id}`,
        onclick: () => loadEarlierTranscript(s.session_id, TRANSCRIPT_PAGE_STEP),
      }),
      btn(`加载全部 (${total})`, {
        cls: 'ghost xs',
        busyKey: `trall:${s.session_id}`,
        onclick: () => loadAllTranscript(s.session_id),
      })));
}

function parseSystemCardText(rawText) {
  const txt = String(rawText || '').trim();
  const nl = txt.indexOf('\n');
  const firstLine = (nl >= 0 ? txt.slice(0, nl) : txt).trim();
  const rest = (nl >= 0 ? txt.slice(nl + 1) : '').trim();
  let tag = '系统消息';
  let body = txt;
  if (firstLine.startsWith('[后台任务') && firstLine.endsWith(']')) {
    tag = firstLine.slice(1, -1);
    body = rest || txt;
  } else if (firstLine === '[Swarm 通知]') {
    tag = 'Swarm 通知';
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

function classifyCollapsibleAssistant(msg, hasLaterAssistantInTurn) {
  if (!msg || msg.role !== 'assistant') return null;
  const raw = String(msg.text || '');
  if (!raw.trim()) return null;
  const lines = raw.replace(/\r\n?/g, '\n').split('\n').length;
  const chars = raw.length;
  const kind = msg.kind || '';
  if (kind === 'plan') return { tag: '执行计划', lines };
  if (kind === 'agent_message') {
    return { tag: raw.trimStart().startsWith('[群组消息]') ? '群组消息' : '代理通信', lines };
  }
  if (kind === 'final') return null;
  if (kind === 'intermediate') {
    return lines >= INTERMEDIATE_COLLAPSE_LINES || chars >= INTERMEDIATE_COLLAPSE_CHARS
      ? { tag: '过程输出', lines }
      : null;
  }
  if (hasLaterAssistantInTurn && (lines >= LEGACY_MID_COLLAPSE_LINES || chars >= LEGACY_MID_COLLAPSE_CHARS)) {
    return { tag: '过程输出', lines };
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
  if (!cleaned.length) return '中间过程输出';
  if (cleaned.length >= 2 && cleaned[0].length <= 24) return `${cleaned[0]} · ${cleaned[1]}`;
  return cleaned[0];
}

function renderMessageCard(s, msg, idx, userIdx = null, hasLaterAssistantInTurn = false) {
  if (msg.role === 'system') {
    const sysKey = `sys:${s.session_id}:${idx}:${msg.ts_ms || 0}`;
    const { tag, preview, body } = parseSystemCardText(msg.text);
    return h('article', { class: 'msg-card system' },
      h('details', { class: 'msg-sys-details', 'data-k': sysKey, open: UI.open.has(sysKey) },
        h('summary', { class: 'msg-sys-summary' },
          span('msg-sys-tag', tag),
          span('msg-sys-preview trunc grow', preview || '系统通知'),
          msg.ts_ms ? span('tabular dim', clock(msg.ts_ms)) : null),
        body ? h('div', { class: 'msg-sys-body' }, renderMarkdown(body)) : null));
  }
  const isUser = msg.role === 'user';
  const tools = Array.isArray(msg.tools) ? msg.tools.filter(Boolean) : [];
  const tKey = `tools:${s.session_id}:${idx}:${msg.ts_ms || 0}`;
  const agentName = AGENT_DISPLAY[s.agent] || s.agent || 'Agent';
  const canQuote = !isUser && Boolean(msg.text && msg.text.trim()) && !s.is_subagent && s.state !== 'ended';
  let cardEl;
  const quoteBtn = canQuote ? h('button', {
    type: 'button',
    class: 'msg-quote-btn',
    title: '引用选中的片段或整条回复到输入框（继续在其下方提问）',
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
  }, '引用') : null;
  const collapsible = classifyCollapsibleAssistant(msg, hasLaterAssistantInTurn);
  if (collapsible) {
    const midKey = `mid:${s.session_id}:${msg.ts_ms || idx}:${hashStr(String(msg.text || '').slice(0, 120))}`;
    const metaText = `${collapsible.lines} 行${tools.length ? ` · ${tools.length} 工具` : ''}`;
    const collapseBtn = collapsible.lines >= 16 ? h('button', {
      type: 'button',
      class: 'msg-mid-collapse-btn',
      onclick: () => {
        detailsEl.open = false;
        UI.open.delete(midKey);
        detailsEl.scrollIntoView({ block: 'nearest' });
      },
    }, `收起${collapsible.tag}`) : null;
    const detailsEl = h('details', { class: 'msg-mid-details', 'data-k': midKey, open: UI.open.has(midKey) },
      h('summary', { class: 'msg-mid-summary' },
        h('i', { class: `dot agent-dot-${s.agent || 'x'}` }),
        span('msg-mid-tag', collapsible.tag),
        span('msg-mid-preview trunc grow', summarizeCollapsePreview(msg.text)),
        span('msg-mid-meta tabular', metaText),
        msg.ts_ms ? span('tabular dim', clock(msg.ts_ms)) : null),
      h('div', { class: 'msg-mid-body' },
        h('div', { class: 'msg-mid-body-head' },
          h('span', { class: 'row' },
            span(`tag a-${s.agent || 'x'}`, agentName),
            modelEffortLabel(s.model, s.effort) ? span('mono muted', modelEffortLabel(s.model, s.effort)) : null),
          h('span', { class: 'row' }, quoteBtn)),
        renderMarkdown(msg.text),
        tools.length ? h('details', { class: 'msg-tools', 'data-k': tKey, open: UI.open.has(tKey) },
          h('summary', null, `调用了 ${tools.length} 个工具`),
          h('div', { class: 'msg-tool-list' }, tools.map((t) => span('tag mono', t)))) : null,
        collapseBtn));
    cardEl = h('article', { class: 'msg-card intermediate' }, detailsEl);
    return cardEl;
  }
  cardEl = h('article', {
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
    h('span', { class: 'row' }, quoteBtn, msg.ts_ms ? span('tabular dim', clock(msg.ts_ms)) : null)),
  msg.text ? renderMarkdown(msg.text) : (tools.length ? h('div', { class: 'muted', style: 'font-size:13px' }, '正在调用工具…') : null),
  tools.length ? h('details', { class: 'msg-tools', 'data-k': tKey, open: UI.open.has(tKey) },
    h('summary', null, `调用了 ${tools.length} 个工具`),
    h('div', { class: 'msg-tool-list' }, tools.map((t) => span('tag mono', t)))) : null);
  return cardEl;
}

function renderTimelineCards(s, list) {
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
  let uIdx = 0;
  return list.map((msg, idx) => renderMessageCard(
    s,
    msg,
    idx,
    msg.role === 'user' ? ++uIdx : null,
    hasLaterAssistant[idx],
  ));
}

export function buildOutputBody(s) {
  const mode = s.agent === 'shell' ? 'raw' : UI.outputMode;
  const pane = s.pane;
  const curPvMode = pane ? pvModeOf(pane) : PREVIEW_STEPS[0];
  const pvKey = pane ? pvKeyOf(pane, curPvMode) : null;
  const pv = pvKey ? S.previews.get(pvKey) : null;
  const tr = S.transcripts.get(s.session_id);
  const glyphSig = `${UI.termGlyphMode}:${effectiveGlyphMode()}`;
  const errSig = `${s.state}:${hashStr(s.last_error || '')}`;
  const alertBanner = sessionAlertBanner(s);

  if (mode === 'raw') {
    if (!pane) {
      return {
        sig: `raw:nopane:${s.session_id}:${errSig}`,
        sourceLabel: '原始终端',
        node: h('div', { class: 'raw-shell stack-sm' }, alertBanner, emptyState('该会话未绑定终端 pane', s.last_message || '无法抓屏预览原始终端。')),
      };
    }
    if (!pv || (pv.state === 'loading' && !pv.text)) {
      return {
        sig: `raw:loading:${pane}:${curPvMode}:${errSig}`,
        sourceLabel: '原始终端',
        node: h('div', { class: 'raw-shell stack-sm' }, alertBanner, skeleton(6)),
      };
    }
    if (pv.state === 'err' && !pv.text) {
      return {
        sig: `raw:err:${pane}:${curPvMode}:${pv.err}:${errSig}`,
        sourceLabel: '原始终端',
        node: h('div', { class: 'raw-shell stack-sm' },
          alertBanner,
          buildRawToolbar(pane, curPvMode, 0, pv),
          errorBanner(`读取终端失败：${pv.err}`, '重试获取终端', () => loadPreview(pane, { mode: curPvMode, silent: false })),
          s.last_message ? h('div', { class: 'callout' }, h('div', { class: 'sec-h' }, '最近事件摘要'), renderMarkdown(s.last_message)) : null),
      };
    }
    if (!pv.text.trim()) {
      return {
        sig: `raw:empty:${pane}:${curPvMode}:${errSig}`,
        sourceLabel: '原始终端',
        node: h('div', { class: 'raw-shell stack-sm' }, alertBanner, buildRawToolbar(pane, curPvMode, 0, pv), emptyState('终端暂无可见输出', s.last_message || '向下方输入框发送一条消息试试。')),
      };
    }
    const cleanRaw = formatTerminalText(pv.text);
    const actualLines = cleanRaw ? cleanRaw.split('\n').length : 0;
    return {
      sig: `raw:ok:${pane}:${curPvMode}:${pv.hash}:${glyphSig}:${errSig}`,
      sourceLabel: '原始终端',
      node: h('div', { class: 'raw-shell stack-sm' },
        alertBanner,
        buildRawToolbar(pane, curPvMode, actualLines, pv),
        buildRawOlderBar(pane, pv),
        renderChunkedTerminal(cleanRaw)),
    };
  }

  if (tr?.state === 'ok' && tr.source === 'transcript' && tr.messages.length > 0) {
    const tailText = pv?.text ? formatTerminalText(pv.text).trim().split('\n').slice(-LIVE_TAIL_LINES).join('\n') : '';
    const tailKey = `tail:${s.session_id}`;
    const autoOpenTail = sessionNeedsLiveTail(s, tr);
    const historyBar = buildHistoryBar(s, tr);
    const timeline = h('div', { class: 'timeline' },
      alertBanner,
      historyBar,
      renderTimelineCards(s, tr.messages),
      tailText ? h('details', { class: 'live-tail', 'data-k': tailKey, open: UI.open.has(tailKey) || autoOpenTail },
        h('summary', null, `当前终端实时行（${paneDisplayId(s) || 'pane'} · 最近 ${LIVE_TAIL_LINES} 行）`),
        h('pre', { class: 'term-pre' }, tailText)) : null);
    return {
      sig: `read:tr:${s.session_id}:${tr.hash}:${hashStr(tailText)}:${glyphSig}:${errSig}:${autoOpenTail ? 1 : 0}`,
      sourceLabel: '对话记录',
      node: timeline,
    };
  }

  if (!pane) {
    if (tr?.state === 'loading') return { sig: `read:trload:${s.session_id}:${errSig}`, sourceLabel: '阅读视图', node: h('div', { class: 'timeline' }, alertBanner, skeleton(5)) };
    if (s.last_message) return { sig: `read:last:${s.session_id}:${hashStr(s.last_message)}:${errSig}`, sourceLabel: '最近摘要', node: h('div', { class: 'timeline' }, alertBanner, renderMessageCard(s, { role: 'assistant', text: s.last_message }, 0)) };
    return { sig: `read:empty:${s.session_id}:${errSig}`, sourceLabel: '阅读视图', node: h('div', { class: 'timeline' }, alertBanner, emptyState('暂无可阅读内容', '该会话尚未产生对话记录且未绑定终端 pane。')) };
  }
  if ((!pv || (pv.state === 'loading' && !pv.text)) && (!tr || tr.state === 'loading')) {
    return { sig: `read:loading:${s.session_id}:${curPvMode}:${errSig}`, sourceLabel: '阅读视图', node: h('div', { class: 'timeline' }, alertBanner, skeleton(6)) };
  }
  if (pv?.state === 'err' && !pv.text) {
    return {
      sig: `read:err:${pane}:${curPvMode}:${pv.err}:${errSig}`,
      sourceLabel: '终端 Markdown',
      node: h('div', { class: 'timeline' },
        alertBanner,
        errorBanner(`读取终端失败：${pv.err}`, '重试获取终端', () => loadSessionOutput(s, { silent: false, forcePreview: true })),
        s.last_message ? renderMessageCard(s, { role: 'assistant', text: s.last_message }, 0) : null),
    };
  }
  if (!pv?.text?.trim()) {
    return { sig: `read:empty:${pane}:${curPvMode}:${errSig}`, sourceLabel: '终端 Markdown', node: h('div', { class: 'timeline' }, alertBanner, emptyState('终端暂无可见输出', s.last_message || '向下方输入框发送一条消息试试。')) };
  }
  const turns = parseTerminalTurns(pv.text, s.agent);
  const fallbackToolbar = buildRawToolbar(pane, curPvMode, pv.text.split('\n').length, pv);
  if (!turns.length) {
    return {
      sig: `read:rawfallback:${pane}:${curPvMode}:${pv.hash}:${glyphSig}:${errSig}`,
      sourceLabel: '终端 Markdown',
      node: h('div', { class: 'timeline' },
        alertBanner,
        fallbackToolbar,
        buildRawOlderBar(pane, pv),
        h('article', { class: 'msg-card assistant' }, renderChunkedTerminal(formatTerminalText(pv.text)))),
    };
  }
  return {
    sig: `read:pv:${pane}:${curPvMode}:${pv.hash}:${glyphSig}:${errSig}`,
    sourceLabel: '终端 Markdown',
    node: h('div', { class: 'timeline' }, alertBanner, fallbackToolbar, buildRawOlderBar(pane, pv), renderTimelineCards(s, turns)),
  };
}

export function jumpToPrompt(userIdx) {
  if (UI.outputMode !== 'reading') { UI.outputMode = 'reading'; APP.rerender(); }
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
  APP.rerender();
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
  const paneBadge = paneDisplayId(s);
  const mux = muxKindOf(s);
  const card = h('button', {
    type: 'button',
    class: `ws-item${sel ? ' sel' : ''}`,
    'data-sid': s.session_id,
    onclick: () => APP.openSession(s.session_id),
  },
  h('div', { class: 'ws-item-top' },
    h('i', { class: `dot dot-${s.state}`, title: stateText(s.state) }),
    agentTag(s.agent),
    h('span', { class: 'ws-item-target grow', title: primaryTitle }, primaryTitle),
    paneBadge ? h('span', { class: 'tag host-pfx', title: paneBadge }, paneBadge) : null),
  h('div', { class: 'ws-item-sub' },
    h('span', { class: 'trunc grow' }, subMeta),
    mux === 'zellij' ? span('tag mux-zj', 'zellij') : null,
    s.discovered_only && mux !== 'zellij' ? span('tag legacy', 'tmux 发现') : null,
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
      type: 'button',
      class: 'ws-sub-toggle',
      onclick: () => {
        if (UI.open.has(subOpenKey)) UI.open.delete(subOpenKey);
        else UI.open.add(subOpenKey);
        APP.rerender();
      },
    }, `↳ ${subs.length} 个子会话 ${subsOpen ? '▾' : '▸'}`),
    subsOpen ? subs.map((c) => h('button', {
      type: 'button',
      class: `ws-sub-item${curId === c.session_id ? ' sel' : ''}`,
      onclick: () => APP.openSession(c.session_id),
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
    placeholder: '搜索主机、会话、窗口或目录…',
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
  const sidebar = h('aside', { class: 'ws-sidebar', 'aria-label': '会话列表' },
    h('div', { class: 'ws-side-head' },
      h('div', { class: 'ws-side-top' },
        h('div', { class: 'ws-side-title' }, '会话列表', sideCount),
        btn('+ 新建交互会话', { cls: 'sm', title: '在 tmux 中新开 claude / codex / agy 交互会话（快捷键 n）', onclick: () => APP.openSpawnModal() })),
      h('div', { class: 'ws-search-row' }, sideSearch, sideToggleSlot),
      h('div', { class: 'row' },
        sideChips,
        h('label', { class: 'row muted', style: 'font-size:12px;margin-left:auto;cursor:pointer' }, sideShowEnded, '含已结束'))),
    sideList);

  const headEl = h('header', { class: 'ws-chat-head' });
  const crumbEl = h('div', { class: 'sub-crumb ws-hidden' });
  const reopenBtn = h('button', {
    type: 'button',
    class: 'outline-reopen-btn ws-hidden',
    title: '展开提问目录',
    onclick: () => setOutlineCollapsed(false),
  });
  const vp = h('div', {
    class: 'term-viewport',
    onscroll: (e) => syncScrollLatestBtn(e.currentTarget),
  });
  const latestBtn = h('button', {
    type: 'button',
    class: 'scroll-latest-btn',
    onclick: () => { vp.scrollTo({ top: vp.scrollHeight, behavior: 'smooth' }); },
  }, '↓ 回到最新');
  const chatStage = h('div', { class: 'chat-stage' }, vp, latestBtn);
  const slashEl = h('div', { class: 'slash-menu ws-hidden' });
  const modeBtns = [['direct', '直接发送'], ['feature', '新功能协作'], ['debug', '两速查错']].map(([k, l]) => h('button', {
    type: 'button',
    role: 'tab',
    class: 'mode-btn',
    'data-mode': k,
    onclick: () => { UI.composerMode = k; closeSlashMenu(); APP.rerender(); },
  }, l));
  const modeHint = h('span', { class: 'dim', style: 'font-size:12px' });
  const modeBar = h('div', { class: 'mode-bar' },
    h('div', { class: 'mode-seg', role: 'tablist', 'aria-label': '输入模式' }, modeBtns),
    modeHint);
  const attachBar = h('div', { class: 'attach-bar ws-hidden' });
  const ta = h('textarea', {
    id: 'composer-input',
    rows: '2',
    class: 'composer-ta',
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
          ? { kind: 'image', title: a.title, path: a.path, ...(a.token ? { token: a.token } : {}) }
          : { kind: 'text', title: a.title, content: a.content, ...(a.token ? { token: a.token } : {}) }));
      }
      const r = await api('POST', '/api/dispatch', payload);
      UI.drafts.delete(dk);
      UI.attachments.delete(sid);
      if (comp.sid === sid) {
        ta.value = '';
        ta.style.height = 'auto';
        autoGrow(ta);
        comp.lastSel = { sid, start: 0, end: 0 };
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
    saveComposerSelection(ta);
    autoGrow(ta);
    if (comp.agent !== 'shell') updateSlashFromInput(ta, comp.agent);
  });
  ta.addEventListener('click', () => {
    saveComposerSelection(ta);
    if (comp.agent !== 'shell') updateSlashFromInput(ta, comp.agent);
  });
  ta.addEventListener('keyup', (e) => {
    saveComposerSelection(ta);
    if (comp.agent !== 'shell' && ['ArrowLeft', 'ArrowRight', 'Home', 'End'].includes(e.key)) updateSlashFromInput(ta, comp.agent);
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
      if (item) toast(`已折叠为 ${item.token}（${item.lines} 行 · ${fmtBytes(item.bytes)}），发送时将按原位置展开`, 'ok');
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
    type: 'file',
    accept: 'image/*',
    multiple: true,
    class: 'ws-hidden',
    'aria-label': '选择图片上传',
    onchange: () => {
      if (imgInput.files?.length) uploadImageFiles(comp.sid, imgInput.files);
      imgInput.value = '';
    },
  });
  const imgBtn = h('button', {
    type: 'button',
    class: 'btn ghost sm',
    title: '上传图片或截图（支持粘贴与拖拽）',
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
  const comp = { dock, card, slashEl, readonlyHint, modeBtns, modeHint, attachBar, directRow, ta, imgBtn, sendBtn, pipeSlot, sid: null, agent: null, lastSel: null };
  const convCol = h('div', { class: 'ws-conv-col' }, reopenBtn, chatStage, dock);
  const bodySplit = h('div', { class: 'ws-body-split' }, convCol);
  const emptyMain = emptyState('选择一个会话开始对话', '从左侧点击任意正在运行的 tmux / Zellij 会话，即可查看实时终端并直接发送指令。',
    btn('新建交互会话', { cls: 'primary', onclick: () => APP.openSpawnModal() }));
  emptyMain.classList.add('ws-hidden');
  const main = h('section', { class: 'ws-main' }, headEl, crumbEl, bodySplit, emptyMain);
  const grid = h('div', { class: 'ws-grid' }, sidebar, main);
  const errSlot = h('div', { class: 'ws-hidden', style: 'padding:10px 16px 0' });
  return { errSlot, grid, sideCount, sideSearch, sideToggleSlot, sideChips, sideShowEnded, sideList, headEl, crumbEl, bodySplit, convCol, reopenBtn, vp, emptyMain, comp, rail: null, railSig: '' };
}

export function mountOrPatchWorkspace(el) {
  if (!UI.wsShell) UI.wsShell = createWorkspaceShell();
  const W = UI.wsShell;
  if (W.grid.parentNode !== el) {
    el.replaceChildren(W.errSlot, W.grid);
  }
  if (S.loadError) {
    W.errSlot.classList.remove('ws-hidden');
    W.errSlot.replaceChildren(errorBanner(`状态同步异常：${S.loadError}`, '重试同步', loadAll));
  } else {
    W.errSlot.classList.add('ws-hidden');
    W.errSlot.replaceChildren();
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
  const allCollapsed = allCollapseKeys.length > 0 && allCollapseKeys.every(({ cKey }) => UI.collapsedGroups.has(cKey));
  const toggleAllGroups = () => {
    if (allCollapsed) {
      UI.collapsedGroups.clear();
      try { localStorage.setItem(NON_TMUX_OPEN_KEY, '1'); } catch { /* ignore */ }
    } else {
      allCollapseKeys.forEach(({ cKey }) => UI.collapsedGroups.add(cKey));
      try { localStorage.setItem(NON_TMUX_OPEN_KEY, '0'); } catch { /* ignore */ }
    }
    saveCollapsedGroups();
    APP.rerender();
  };

  W.sideCount.textContent = String(shown.length);
  if (document.activeElement !== W.sideSearch && W.sideSearch.value !== draft('ws:filter')) W.sideSearch.value = draft('ws:filter');
  W.sideShowEnded.checked = UI.showEnded;
  W.sideToggleSlot.replaceChildren(allCollapseKeys.length > 1 ? btn(allCollapsed ? '全部展开' : '全部收起', { cls: 'ghost xs', onclick: toggleAllGroups }) : '');
  const filterAgents = (roots.some((s) => s.agent === 'shell') || UI.agentFilter === 'shell') ? [...AGENTS, 'shell'] : AGENTS;
  W.sideChips.replaceChildren(...['', ...filterAgents].map((a) => h('button', {
    type: 'button',
    class: `chip${UI.agentFilter === a ? ' on' : ''}`,
    onclick: () => { UI.agentFilter = a; APP.schedule(); },
  }, a || '全部')));

  const prevSideTop = W.sideList.scrollTop;
  const visibleHostBlocks = hostOrder
    .map((hid) => hostMap.get(hid))
    .filter((hb) => hb.total > 0 || (S.hosts.size > 1 && S.hosts.has(hb.id)));

  if (!shown.length && S.hosts.size <= 1) {
    W.sideList.replaceChildren(emptyState(
      roots.length ? '没有匹配的会话' : '暂未发现活跃会话',
      roots.length ? '尝试清空搜索关键词或切换 Agent 过滤条件。' : '启动中的 tmux / Zellij 会话会自动同步到这里，也可直接新建一个交互会话。',
      !roots.length ? btn('新建交互会话', { cls: 'primary sm', onclick: () => APP.openSpawnModal() }) : null,
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
      const hostHead = h('div', { class: 'ws-host-head' },
        h('div', { class: 'ws-host-left' },
          h('i', { class: `dot dot-${hStatus}`, title: `${hName} · ${hStatus}${hRec?.active_endpoint ? ` (${hRec.active_endpoint})` : ''}` }),
          h('span', { class: 'ws-host-name', title: `${hName} (${hb.id})` }, hName),
          span('ws-host-id', hb.id),
          hKind !== 'local' ? span('tag', hKind === 'ssh' ? 'SSH' : 'Peer') : null),
        h('div', { class: 'row-nowrap', style: 'gap:4px' },
          h('button', {
            type: 'button',
            class: 'ws-host-rename',
            'data-host': hb.id,
            title: `重命名主机 ${hName}（${hb.id}）`,
            'aria-label': `重命名主机 ${hb.id}`,
            onclick: () => APP.openRenameHostModal(hb.id),
          }, '✎'),
          span('tag tabular', hb.total)));

      const groupNodes = sortedGroups.map(([g, list]) => {
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
            h('span', { class: 'ws-group-name trunc' }, g),
            isCollapsed && attnSess ? h('i', { class: `dot dot-${attnSess.state}`, title: stateText(attnSess.state) }) : null),
          span('ws-group-cnt tabular', list.length)),
          !isCollapsed ? h('div', { class: 'ws-group-body' }, list.map((s) => sidebarSessionItem(s))) : null);
      });
      return h('div', { class: 'ws-host-block', 'data-host-id': hb.id },
        hostHead,
        hStatus === 'degraded' && hRec?.error ? h('div', { class: 'ws-host-err' }, `连接降级：${hRec.error}`) : null,
        groupNodes.length ? groupNodes : h('div', { class: 'dim', style: 'font-size:12px;padding:2px 8px' }, '暂无活跃会话'));
    }));
  }
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
  const paneBadge = paneDisplayId(s);
  const titleTip = [hostDisplayName(hostOf(s), s), dir, s.tmux_target, paneBadge, meLabel].filter(Boolean).join(' · ');
  const parent = s.is_subagent && s.parent_session_id ? S.sessions.get(s.parent_session_id) : null;
  const displayTitle = s.is_subagent ? subagentDisplayName(s) : sessionLabel(s);
  const isShell = s.agent === 'shell';
  const effectiveMode = isShell ? 'raw' : UI.outputMode;
  const viewToggle = isShell
    ? h('div', { class: 'view-seg', role: 'tablist', 'aria-label': '显示模式' },
      h('button', { type: 'button', role: 'tab', class: 'on', onclick: () => { if (s.pane) loadPreview(s.pane, { mode: pvModeOf(s.pane), silent: false }); } }, '终端'))
    : h('div', { class: 'view-seg', role: 'tablist', 'aria-label': '显示模式' },
      h('button', { type: 'button', role: 'tab', class: UI.outputMode === 'reading' ? 'on' : '', onclick: () => { UI.outputMode = 'reading'; APP.rerender(); } }, '阅读'),
      h('button', { type: 'button', role: 'tab', class: UI.outputMode === 'raw' ? 'on' : '', onclick: () => { UI.outputMode = 'raw'; if (s.pane) loadPreview(s.pane, { mode: pvModeOf(s.pane), silent: true }); APP.rerender(); } }, '终端'));
  W.headEl.replaceChildren(
    h('div', { class: 'ws-head-left', title: titleTip },
      h('h2', { class: 'ws-head-title' }, displayTitle),
      paneBadge ? span('tag host-pfx', paneBadge) : null,
      agentTag(s.agent),
      muxKindOf(s) === 'zellij' ? span('tag mux-zj', 'zellij') : null,
      meLabel ? span('tag mono ws-model-pill', meLabel) : null,
      statusInd(s.state, stateText(s.state))),
    h('div', { class: 'ws-head-right' },
      buildTelemetryStrip(s),
      viewToggle,
      btn('详情', { cls: 'sm', title: '展开右侧检查抽屉（切到终端 / 刷新 / 性能与上下文 / 元数据）', onclick: () => APP.openDrawer('session', { sessionId: s.session_id }) })),
  );

  if (s.is_subagent) {
    W.crumbEl.classList.remove('ws-hidden');
    W.crumbEl.replaceChildren(
      h('span', { class: 'row' }, span('tag', '子会话'), h('span', null, `所属 ${parent ? sessionLabel(parent) : s.parent_session_id}`)),
      s.parent_session_id ? btn('← 返回主会话', { cls: 'ghost xs', onclick: () => APP.openSession(s.parent_session_id) }) : '',
    );
  } else {
    W.crumbEl.classList.add('ws-hidden');
  }

  if (!isShell && !S.transcripts.has(s.session_id)) loadTranscript(s.session_id, { silent: false });
  if (s.pane && !S.previews.has(pvKeyOf(s.pane))) loadPreview(s.pane, { mode: pvModeOf(s.pane), silent: false });
  if (!isShell) loadSlashCommands(s.agent);

  const built = buildOutputBody(s);
  const sameView = W.vp.dataset.sid === s.session_id && W.vp.dataset.mode === effectiveMode;
  if (!sameView) {
    W.vp.dataset.sid = s.session_id;
    W.vp.dataset.mode = effectiveMode;
    W.vp.dataset.sig = built.sig;
    W.vp.className = `term-viewport${effectiveMode === 'raw' ? ' raw-mode' : ''}`;
    W.vp.replaceChildren(built.node);
    scrollViewportToBottom(W.vp);
    syncScrollLatestBtn(W.vp);
  } else if (W.vp.dataset.sig !== built.sig) {
    const prevHeight = W.vp.scrollHeight;
    const wasLoading = W.vp.dataset.sig?.includes(':loading:');
    const atBottom = wasLoading || (prevHeight - W.vp.scrollTop - W.vp.clientHeight < 64);
    const prevTop = W.vp.scrollTop;
    const expandingReading = UI.loadingEarlierSid === s.session_id && effectiveMode === 'reading';
    const expandingRaw = s.pane && UI.loadingEarlierPane === s.pane && effectiveMode === 'raw';
    W.vp.dataset.sig = built.sig;
    W.vp.className = `term-viewport${effectiveMode === 'raw' ? ' raw-mode' : ''}`;
    W.vp.replaceChildren(built.node);
    if (expandingReading || expandingRaw) {
      if (expandingReading) UI.loadingEarlierSid = null;
      if (expandingRaw) UI.loadingEarlierPane = null;
      W.vp.scrollTop = Math.max(0, prevTop + (W.vp.scrollHeight - prevHeight));
    } else if (atBottom) {
      scrollViewportToBottom(W.vp);
    } else {
      W.vp.scrollTop = prevTop;
    }
    syncScrollLatestBtn(W.vp);
  }

  const prompts = isShell ? [] : collectUserPrompts(s);
  const railSig = `${s.session_id}|${effectiveMode}|${UI.outlineCollapsed}|${prompts.map((p) => `${p.idx}:${p.text}`).join('|')}`;
  if (W.railSig !== railSig || (!UI.outlineCollapsed && prompts.length > 0 && effectiveMode === 'reading' && !W.rail?.isConnected)) {
    W.railSig = railSig;
    const showReopen = effectiveMode === 'reading' && prompts.length > 0 && UI.outlineCollapsed;
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
  const readOnlyMsg = s.is_subagent
    ? '只读子会话 · 无法直接在此发送消息，请返回主会话操作'
    : (s.state === 'ended' ? '该会话已结束 · 请切换至活跃会话发送指令' : '');
  if (readOnlyMsg) {
    C.readonlyHint.textContent = readOnlyMsg;
    C.readonlyHint.classList.remove('ws-hidden');
  } else {
    C.readonlyHint.classList.add('ws-hidden');
  }
  const effCompMode = isShell ? 'direct' : UI.composerMode;
  for (const b of C.modeBtns) {
    b.classList.toggle('ws-hidden', isShell && b.dataset.mode !== 'direct');
    b.classList.toggle('on', b.dataset.mode === effCompMode);
    b.disabled = !!readOnlyMsg;
  }
  C.modeHint.textContent = readOnlyMsg || (isShell
    ? '发送终端命令或输入 · Cmd+Enter 执行'
    : (effCompMode === 'direct' ? '输入 / 唤出命令 · Cmd+Enter 发送' : '可选模板 · 留空使用默认配置'));
  if (effCompMode === 'direct' || readOnlyMsg) {
    C.directRow.classList.remove('ws-hidden');
    C.pipeSlot.classList.add('ws-hidden');
    if (C.sid !== s.session_id) {
      if (C.sid) UI.drafts.set(`ws:msg:${C.sid}`, C.ta.value);
      C.sid = s.session_id;
      C.agent = s.agent;
      C.ta.dataset.draft = `ws:msg:${s.session_id}`;
      C.ta.value = draft(`ws:msg:${s.session_id}`);
      C.lastSel = { sid: s.session_id, start: C.ta.value.length, end: C.ta.value.length };
      C.ta.style.height = 'auto';
      autoGrow(C.ta);
      closeSlashMenu();
    } else {
      C.agent = s.agent;
      if (isShell) closeSlashMenu();
    }
    const ph = readOnlyMsg || (isShell
      ? `输入终端命令发送到 ${sessionLabel(s)}…（Cmd+Enter 执行）`
      : `发消息给 ${sessionLabel(s)}…（输入 / 唤出命令，Cmd+Enter 发送）`);
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
