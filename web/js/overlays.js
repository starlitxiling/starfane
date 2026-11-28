// starfane web console — Right Inspection Drawer & Modals (Spawn, Pipeline, Auth, Rename Host, Close Window)
import { AGENTS, NON_TMUX_GROUP, PLAYBOOK_STAGES, SEVERITIES, TOKEN_KEY, effortLabel } from './constants.js';
import { L, getLang, localeTag } from './i18n.js';
import {
  $,
  agentTag,
  ago,
  btn,
  cacheHitRate,
  cacheHitTone,
  clock,
  codeRefList,
  copyText,
  ctxBar,
  ctxPctOf,
  dur,
  emptyState,
  errText,
  errorBanner,
  evBlocks,
  field,
  fmtBytes,
  fmtHitRate,
  fmtMs,
  fmtTokens,
  h,
  isFin,
  kvGrid,
  metricsSrcLabel,
  modelTag,
  pct,
  plainList,
  selectField,
  sessionTotalTokens,
  shorthand,
  skeleton,
  span,
  statusInd,
  toast,
  tpsOf,
  ttftOf,
  ttftTone,
} from './dom.js';
import {
  ApiError,
  act,
  api,
  artifact,
  closeSessionApi,
  connectSSE,
  isTmuxSessionsLoading,
  loadAll,
  loadHandoffs,
  loadModels,
  loadPreview,
  loadSessionOutput,
  loadTmuxSessions,
  loadUsage,
  pauseHostRetryApi,
  probeHostApi,
  refreshReview,
  renameHostApi,
  resumeSessionApi,
  suspendSessionApi,
} from './api.js';
import { expandAttachmentToInput, queueParse, removeAttachment } from './composer.js';
import { mdProse, renderMarkdown } from './markdown.js';
import {
  APP,
  S,
  UI,
  currentSession,
  draft,
  effortsForModel,
  enc,
  fmtRtt,
  formatRetryCountdown,
  hostDisplayName,
  hostOf,
  hostRecord,
  hostStatusLabel,
  hydrateSpawnAgent,
  isRemoteHost,
  isTmuxSession,
  knownProjects,
  loadSpawnPrefs,
  modelBaseOf,
  modelsForAgent,
  muxKindOf,
  paneDisplayId,
  pvKeyOf,
  rttTone,
  saveSpawnPrefs,
  sessionLabel,
  short,
  sortedHosts,
  stageName,
  stateText,
  subagentDisplayName,
  subagentsOf,
  tmuxGroupOf,
  upsertPipeline,
} from './store.js';
import { formatTerminalText, renderChunkedTerminal } from './terminal.js';
import { dropReasons, effSev, focusBtn, g2Of, g3Of, sevTagCls } from './views.js';

function rttPill(rttMs, syncMs = null) {
  if (!isFin(rttMs) || rttMs < 0) return null;
  const label = fmtRtt(rttMs);
  if (!label) return null;
  const hasSync = isFin(syncMs) && syncMs >= 0;
  const title = hasSync
    ? L(`网络延迟 ${Math.round(rttMs)}ms · 同步耗时 ${Math.round(syncMs)}ms`, `Network RTT ${Math.round(rttMs)}ms · Sync ${Math.round(syncMs)}ms`)
    : L(`往返延迟 ${label}`, `Round-trip latency ${label}`);
  return h('span', {
    class: `tag mono rtt-pill rtt-${rttTone(rttMs)}`,
    title,
  }, label);
}

function syncPill(syncMs) {
  if (!isFin(syncMs) || syncMs < 0) return null;
  const label = fmtRtt(syncMs);
  if (!label) return null;
  return h('span', {
    class: 'tag mono sync-pill',
    title: L(`远端同步耗时 ${Math.round(syncMs)}ms`, `Remote discovery sync ${Math.round(syncMs)}ms`),
  }, L(`同步 ${label}`, `Sync ${label}`));
}

export function openDrawer(kind, params) {
  UI.mobileSessionsOpen = false;
  UI.drawer = { kind, ...params };
  if (kind === 'session' && params.sessionId) loadHandoffs(params.sessionId);
  if (kind === 'terminal' && params.pane) loadPreview(params.pane, { mode: UI.drawerPvLines, silent: false });
  APP.schedule();
}

export function closeDrawer() {
  UI.drawer = null;
  APP.schedule();
}

export function openSpawnModal() {
  UI.mobileSessionsOpen = false;
  const curHost = hostOf(currentSession());
  const defaultHost = S.hosts.size > 1 && S.hosts.has(curHost) ? curHost : 'local';
  UI.drafts.set('sp:host', defaultHost);
  loadModels();
  loadTmuxSessions(defaultHost);
  const prefs = loadSpawnPrefs();
  const ag = prefs.last_agent || 'claude';
  UI.drafts.set('sp:agent', ag);
  UI.drafts.delete('sp:dir');
  UI.drafts.delete('sp:createDir');
  UI.spawnMissingDir = null;
  UI.spawnErr = '';
  hydrateSpawnAgent(ag, prefs);
  UI.modal = 'spawn';
  APP.schedule();
}

export function canCloseSessionWindow(s) {
  return Boolean(s && !s.is_subagent && String(s.pane || '').trim() && s.state !== 'ended');
}

export function canSuspendSession(s) {
  return Boolean(s && !s.is_subagent && s.agent !== 'shell' && String(s.pane || '').trim() && s.state !== 'ended' && s.state !== 'suspended');
}

export function canResumeSession(s) {
  return Boolean(s && !s.is_subagent && s.agent !== 'shell' && String(s.pane || '').trim() && s.state === 'suspended');
}

export function windowGroupKeyOf(s) {
  if (!s || !s.tmux_target) return null;
  const raw = String(s.tmux_target).trim();
  if (!raw || raw.startsWith('zj:')) return null;
  const colon = raw.lastIndexOf(':');
  if (colon <= 0) return null;
  const afterColon = raw.slice(colon + 1);
  const dot = afterColon.lastIndexOf('.');
  const winPart = dot > 0 ? afterColon.slice(0, dot) : afterColon;
  if (!winPart) return null;
  return `${hostOf(s)}|${raw.slice(0, colon)}:${winPart}`;
}

export function siblingSplitPanesOf(s) {
  const key = windowGroupKeyOf(s);
  if (!key) return [];
  const myPane = String(s.pane || '').trim();
  const out = [];
  for (const other of S.sessions.values()) {
    if (!other || other.session_id === s.session_id) continue;
    if (!canCloseSessionWindow(other)) continue;
    if (myPane && String(other.pane || '').trim() === myPane) continue;
    if (windowGroupKeyOf(other) === key) out.push(other);
  }
  return out;
}

export function openRenameHostModal(hostId) {
  UI.renameHostId = hostId;
  UI.drafts.set('host:rename', hostDisplayName(hostId));
  UI.modal = 'rename-host';
  APP.schedule();
}

export function openCloseSessionModal(sessionId) {
  if (!sessionId) return;
  const s = S.sessions.get(sessionId);
  if (!canCloseSessionWindow(s)) return;
  UI.closeSessionId = sessionId;
  UI.modal = 'close-session';
  APP.schedule();
}

export function closeModal() {
  UI.modal = null;
  UI.renameHostId = null;
  UI.closeSessionId = null;
  UI.suspendFreshSessionId = null;
  UI.spawnMissingDir = null;
  UI.spawnErr = '';
  lastModalKey = null;
  lastModalCoreSig = '';
  lastModalListSig = '';
  APP.schedule();
}

export function renderDrawer() {
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
  let title = L('详情检查器', 'Inspector');
  let sub = '';
  let body = null;
  if (d.kind === 'session') {
    const s = S.sessions.get(d.sessionId);
    title = s ? L(`会话详情 · ${sessionLabel(s)}`, `Session · ${sessionLabel(s)}`) : L('会话详情', 'Session Inspector');
    sub = s ? s.session_id : '';
    body = s ? drawerSession(s) : emptyState(L('会话不存在', 'Session Not Found'), L('该会话可能已被清理。', 'This session may have been cleaned up.'));
  } else if (d.kind === 'terminal') {
    const dMode = UI.drawerPvLines;
    title = L(`完整终端输出 · ${d.title || d.pane}`, `Full Terminal · ${d.title || d.pane}`);
    sub = `${d.pane} (${dMode === 'all' ? L('全部滚动历史', 'Full scrollback') : L(`最近 ${dMode} 行`, `Last ${dMode} lines`)})`;
    const pv = S.previews.get(pvKeyOf(d.pane, dMode));
    const formatted = pv?.text ? formatTerminalText(pv.text) : '';
    body = !pv || pv.state === 'loading' ? skeleton(8)
      : pv.state === 'err' ? errorBanner(L(`读取终端失败：${pv.err}`, `Failed to read terminal: ${pv.err}`), L('重试', 'Retry'), () => loadPreview(d.pane, { mode: dMode }))
      : h('div', { class: 'stack-sm' },
        pv.staleErr ? h('div', { class: 'stale-banner' },
          h('span', { class: 'stale-banner-msg' },
            h('i', { class: 'dot dot-degraded' }),
            span(null, L(`刷新受阻，正显示缓存终端输出：${pv.staleErr}`, `Refresh failed — showing cached terminal output: ${pv.staleErr}`))),
          btn(L('重试', 'Retry'), { cls: 'xs', onclick: () => loadPreview(d.pane, { mode: dMode, silent: false }) })) : null,
        h('div', { class: 'row', style: 'justify-content:space-between' },
          span('muted tabular', L(`共 ${formatted ? formatted.split('\n').length : 0} 行 · 更新于 ${clock(pv.updatedMs)}`, `${formatted ? formatted.split('\n').length : 0} lines · Updated ${clock(pv.updatedMs)}`)),
          h('div', { class: 'row-nowrap', style: 'gap:6px' },
            h('div', { class: 'view-seg' }, [1000, 'all'].map((m) => h('button', {
              type: 'button',
              class: dMode === m ? 'on' : '',
              onclick: () => { UI.drawerPvLines = m; loadPreview(d.pane, { mode: m }); APP.schedule(); },
            }, m === 'all' ? L('全部历史', 'All history') : L(`${m} 行`, `${m} lines`)))),
            btn(L('刷新', 'Refresh'), { cls: 'sm', onclick: () => loadPreview(d.pane, { mode: dMode }) }))),
        formatted ? h('div', { class: 'callout', style: 'background:var(--terminal-bg)' }, renderChunkedTerminal(formatted)) : h('pre', { class: 'code-block' }, L('（无输出）', '(no output)')));
  } else if (d.kind === 'artifact') {
    title = d.title || L(`工件 · ${d.artifactId}`, `Artifact · ${d.artifactId}`);
    sub = d.artifactId;
    body = drawerArtifact(d.artifactId);
  } else if (d.kind === 'finding') {
    const rev = S.reviews.get(d.reviewId);
    const af = rev?.findings?.find((x) => x.finding.id === d.findingId);
    title = af ? L(`评审条目 · ${af.finding.id}`, `Finding · ${af.finding.id}`) : L('评审详情', 'Finding Inspector');
    sub = `${d.reviewId}`;
    body = rev && af ? drawerFinding(rev, af, d.dropped) : skeleton(4);
  } else if (d.kind === 'pipeline') {
    const p = S.pipelines.get(d.pipelineId);
    title = p ? L(`流程元数据 · ${p.id}`, `Pipeline Metadata · ${p.id}`) : L('流程元数据', 'Pipeline Metadata');
    sub = p ? p.goal : '';
    body = p ? drawerPipeline(p) : emptyState(L('未找到流程', 'Pipeline Not Found'));
  } else if (d.kind === 'outline') {
    const s = S.sessions.get(d.sessionId) || currentSession();
    const prompts = s ? APP.collectUserPrompts(s) : [];
    title = L(`提问目录（${prompts.length}）`, `Prompt Outline (${prompts.length})`);
    sub = s ? sessionLabel(s) : '';
    body = s ? drawerOutline(s, prompts) : emptyState(L('会话不存在', 'Session Not Found'), L('该会话可能已被清理。', 'This session may have been cleaned up.'));
  } else if (d.kind === 'paste') {
    const list = UI.attachments.get(d.sessionId) || [];
    const att = list.find((x) => x.id === d.attachmentId);
    title = att ? att.title : L('附件预览', 'Attachment Preview');
    const tokPre = att?.token ? `${att.token} · ` : '';
    sub = att ? (att.kind === 'image' ? `${tokPre}${att.mime || 'image'} · ${fmtBytes(att.bytes)}` : `${tokPre}${L(`${att.lines} 行`, `${att.lines} lines`)} · ${fmtBytes(att.bytes)}`) : '';
    body = att ? drawerPaste(d.sessionId, att) : emptyState(L('附件已移除', 'Attachment Removed'), L('该附件已被发送或删除。', 'This attachment was sent or deleted.'));
  }
  root.replaceChildren(
    h('div', { class: 'drawer-backdrop', onclick: closeDrawer }),
    h('aside', { class: 'drawer-panel', role: 'dialog', 'aria-label': title },
      h('div', { class: 'sheet-handle', 'aria-hidden': 'true' }),
      h('div', { class: 'drawer-head' },
        h('div', { class: 'grow' }, h('h3', null, title), sub ? h('div', { class: 'muted mono', style: 'font-size:12px;margin-top:2px' }, sub) : null),
        btn(L('关闭', 'Close'), { cls: 'ghost sm', onclick: closeDrawer })),
      h('div', { class: 'drawer-body' }, body)));
}

function drawerOutline(s, prompts) {
  if (!prompts.length) {
    return emptyState(
      L('暂无提问记录', 'No Prompts Yet'),
      L('当前会话尚未检测到用户提问记录。', 'No user prompts detected in this session yet.'),
    );
  }
  const fk = `pjump:${s.session_id}`;
  const listEl = h('div', { class: 'outline-drawer-list' });
  const activeIdx = UI.wsShell?.rail?._activeIdx || String(prompts[prompts.length - 1].idx);
  const renderList = () => {
    const q = draft(fk).trim().toLowerCase();
    const filtered = q
      ? prompts.filter((p) => p.text.toLowerCase().includes(q) || `#${p.idx}`.includes(q))
      : prompts;
    if (!filtered.length) {
      listEl.replaceChildren(h('div', { class: 'muted', style: 'padding:16px;text-align:center;font-size:13px' }, L('无匹配提问', 'No matching prompts')));
      return;
    }
    const frag = document.createDocumentFragment();
    for (let i = 0; i < filtered.length; i++) {
      const p = filtered[i];
      const idxStr = String(p.idx);
      const isCur = idxStr === activeIdx;
      frag.appendChild(h('button', {
        type: 'button',
        class: `outline-item outline-drawer-item${isCur ? ' active' : ''}`,
        'data-turn-idx': idxStr,
        onclick: () => {
          closeDrawer();
          APP.jumpToPrompt(p.idx);
        },
      },
      h('div', { class: 'outline-item-top' },
        span('outline-item-idx', `#${p.idx}`),
        p.ts_ms ? span('tabular', clock(p.ts_ms)) : null),
      h('div', { class: 'outline-item-text' }, p.text)));
    }
    listEl.replaceChildren(frag);
  };
  const filterInp = prompts.length > 6 ? (() => {
    const inp = field(fk, {
      class: 'inp',
      type: 'search',
      placeholder: L('过滤提问…', 'Filter prompts…'),
      'aria-label': L('过滤提问', 'Filter prompts'),
      autocomplete: 'off',
    });
    inp.addEventListener('input', renderList);
    return inp;
  })() : null;
  renderList();
  return h('div', { class: 'stack-sm' }, filterInp, listEl);
}

function drawerMetricsCard(m) {
  const pctVal = ctxPctOf(m);
  const tpsVal = tpsOf(m);
  const ttftVal = ttftOf(m);
  const avgTtftVal = isFin(m?.avg_ttft_ms) && m.avg_ttft_ms > 0 ? m.avg_ttft_ms : null;
  const turns = Array.isArray(m?.recent_turns) ? m.recent_turns : [];
  const validCnt = turns.filter((t) => isFin(t?.ttft_ms) && t.ttft_ms > 0).length;
  const hasUsed = isFin(m?.context_used_tokens);
  const hasWin = isFin(m?.context_window_tokens) && m.context_window_tokens > 0;
  let ctxVal = '—';
  if (hasUsed && hasWin && pctVal != null) ctxVal = `${fmtTokens(m.context_used_tokens)} / ${fmtTokens(m.context_window_tokens)} (${Math.round(pctVal)}%)`;
  else if (pctVal != null) ctxVal = `${Math.round(pctVal)}%`;
  else if (hasUsed) ctxVal = fmtTokens(m.context_used_tokens);
  const latestTtftTone = ttftVal != null ? ttftTone(ttftVal) : 'ok';
  const latestTtftCls = latestTtftTone === 'fast' ? ' ttft-fast' : latestTtftTone === 'slow' ? ' ttft-slow' : '';
  const avgTtftTone = avgTtftVal != null ? ttftTone(avgTtftVal) : 'ok';
  const avgTtftCls = avgTtftTone === 'fast' ? ' ttft-fast' : avgTtftTone === 'slow' ? ' ttft-slow' : '';
  const avgTtftText = avgTtftVal != null
    ? (validCnt > 0
      ? L(`${fmtMs(avgTtftVal)} (${validCnt} 次)`, `${fmtMs(avgTtftVal)} (${validCnt} ${validCnt === 1 ? 'turn' : 'turns'})`)
      : fmtMs(avgTtftVal))
    : '—';
  const lastTurn = isFin(m?.last_output_tokens) || isFin(m?.last_duration_ms)
    ? `${isFin(m?.last_output_tokens) ? `${fmtTokens(m.last_output_tokens)} tokens` : '—'} · ${isFin(m?.last_duration_ms) ? fmtMs(m.last_duration_ms) : '—'}`
    : '—';
  const tot = sessionTotalTokens(m);
  const cacheRead = isFin(m?.total_cache_read_tokens) && m.total_cache_read_tokens > 0 ? m.total_cache_read_tokens : null;
  const drawerHit = cacheRead != null ? cacheHitRate(m?.total_input_tokens, cacheRead) : null;
  const totals = isFin(m?.total_input_tokens) || isFin(m?.total_output_tokens)
    ? L(
      `输入 ${fmtTokens(m?.total_input_tokens)} / 输出 ${fmtTokens(m?.total_output_tokens)}${cacheRead != null ? `（缓存 ${fmtTokens(cacheRead)}${drawerHit != null ? ` · 命中率 ${fmtHitRate(drawerHit)}` : ''}）` : ''}`,
      `In ${fmtTokens(m?.total_input_tokens)} / Out ${fmtTokens(m?.total_output_tokens)}${cacheRead != null ? ` (cache ${fmtTokens(cacheRead)}${drawerHit != null ? ` · ${fmtHitRate(drawerHit)} hit` : ''})` : ''}`,
    )
    : '—';
  const srcLine = m?.source && m.source !== 'none'
    ? `${metricsSrcLabel(m.source)}${m.updated_ms ? ` · ${clock(m.updated_ms)}` : ''}`
    : L('暂无遥测数据', 'No telemetry data yet');
  const turnHistoryEl = turns.length > 0
    ? h('div', { class: 'turn-metrics-wrap' },
      h('div', { class: 'turn-metrics-head' },
        L(`每次首字与速度记录（最近 ${turns.length} 次）`, `Per-turn TTFT & speed (last ${turns.length})`)),
      h('div', { class: 'turn-metrics-scroll' },
        h('table', { class: 'turn-metrics-table' },
          h('thead', null,
            h('tr', null,
              h('th', null, L('时间', 'Time')),
              h('th', null, L('首字', 'TTFT')),
              h('th', null, L('速度', 'Speed')),
              h('th', null, L('输出 / 耗时', 'Output / Duration')))),
          h('tbody', null,
            [...turns].reverse().map((t) => {
              const hasTtft = isFin(t?.ttft_ms) && t.ttft_ms > 0;
              const tone = hasTtft ? ttftTone(t.ttft_ms) : 'ok';
              const ttftCls = tone === 'fast' ? 'ttft-fast' : tone === 'slow' ? 'ttft-slow' : '';
              const hasTps = isFin(t?.tps) && t.tps > 0;
              const outText = isFin(t?.output_tokens) ? fmtTokens(t.output_tokens) : '—';
              const durText = isFin(t?.duration_ms) && t.duration_ms > 0 ? fmtMs(t.duration_ms) : '—';
              return h('tr', null,
                h('td', { class: 'muted' }, t?.ts_ms ? clock(t.ts_ms) : '—'),
                h('td', { class: ttftCls || null }, hasTtft ? fmtMs(t.ttft_ms) : '—'),
                h('td', null, hasTps ? `${t.tps.toFixed(1)} tok/s` : '—'),
                h('td', { class: 'muted' }, `${outText} / ${durText}`));
            })))))
    : null;
  return h('div', { class: 'metrics-card' },
    h('div', { class: 'sec-h', style: 'margin:0' }, L('性能与上下文', 'Performance & Context')),
    kvGrid([
      [L('上下文占用', 'Context usage'), h('span', { class: 'row tabular' }, ctxVal, pctVal != null ? ctxBar(pctVal, false) : null)],
      [L('生成速度 (TPS)', 'Generation speed (TPS)'), span('tabular', tpsVal != null ? `${tpsVal.toFixed(1)} tok/s` : '—')],
      [L('最近首字延迟 (TTFT)', 'Latest TTFT'), span(`tabular${latestTtftCls}`, ttftVal != null ? fmtMs(ttftVal) : '—')],
      [L('平均首字延迟', 'Average TTFT'), span(`tabular${avgTtftCls}`, avgTtftText)],
      [L('最近回合', 'Last turn'), span('tabular', lastTurn)],
      [L('会话总消耗', 'Session total'), tot != null ? span('tabular', `${fmtTokens(tot)} tokens`) : span('muted', '—')],
      [L('用量明细', 'Token breakdown'), span('tabular', totals)],
      [L('数据来源', 'Telemetry source'), span('tabular muted', srcLine)],
    ]),
    turnHistoryEl);
}

function drawerSession(s) {
  const subs = subagentsOf(s.session_id);
  const ho = S.handoffs.get(s.session_id);
  const evs = S.activity.filter((e) => e.session_id === s.session_id || (s.pane && e.pane === s.pane)).slice(0, 15);
  const hid = hostOf(s);
  const remote = isRemoteHost(hid);
  const hRec = hostRecord(hid);
  const hStatus = hRec?.status || 'online';
  const hKind = hRec?.kind || (remote ? 'ssh' : 'local');
  const hUnhealthy = remote && (hStatus === 'degraded' || hStatus === 'offline');
  const retryPaused = Boolean(hRec?.retry_paused);
  const retryCd = hUnhealthy && !retryPaused ? formatRetryCountdown(hRec?.next_retry_ms) : '';
  const closable = canCloseSessionWindow(s);
  const suspendable = canSuspendSession(s);
  const resumable = canResumeSession(s);
  const isSplit = closable && siblingSplitPanesOf(s).length > 0;
  return h('div', { class: 'stack' },
    h('div', { class: 'drawer-controls' },
      s.pane ? focusBtn(s.session_id, 'sm') : null,
      (s.state === 'error' || s.state === 'needs_permission' || s.state === 'needs_input' || s.last_error)
        ? btn(L('清除异常状态', 'Clear error state'), { cls: 'sm danger', busyKey: `ack:${s.session_id}`, onclick: () => APP.ackSessionError(s) }) : null,
      btn(L('手动刷新', 'Refresh'), { cls: 'sm', onclick: () => { loadSessionOutput(s, { silent: false, forcePreview: true, force: true }); toast(L('已刷新会话输出', 'Session output refreshed')); } }),
      s.pane ? btn(L('完整终端日志', 'Full Terminal Log'), { cls: 'ghost sm', onclick: () => openDrawer('terminal', { pane: s.pane, title: sessionLabel(s) }) }) : null,
      suspendable ? btn(L('休眠', 'Suspend'), {
        cls: 'sm',
        busyKey: `suspend:${s.session_id}`,
        title: L('结束当前 Agent 进程释放 CPU/内存，保留终端窗口并支持原地恢复原会话', 'Exit the agent process to save CPU/RAM while keeping the pane and conversation resumable'),
        onclick: () => suspendSessionApi(s.session_id),
      }) : null,
      resumable ? btn(s.resume_id ? L('唤醒原会话', 'Wake session') : L('唤醒会话', 'Relaunch session'), {
        cls: 'sm primary',
        busyKey: `resume:${s.session_id}`,
        title: s.resume_id
          ? L('原地唤醒并恢复原会话上下文', 'Wake and resume this conversation in-place')
          : L('在保留的终端窗格中重新启动该 Agent 会话', 'Relaunch a fresh session inside the kept pane'),
        onclick: () => resumeSessionApi(s.session_id),
      }) : null,
      closable ? btn(isSplit ? L('关闭窗格', 'Close pane') : L('关闭窗口', 'Close window'), {
        cls: 'ghost sm danger',
        title: isSplit
          ? L('终止并关闭当前窗格（同窗口其他分屏不受影响）', 'Terminate and close this pane (other split panes stay open)')
          : L('终止并关闭该终端窗口', 'Terminate and close this terminal window'),
        onclick: () => openCloseSessionModal(s.session_id),
      }) : null,
      h('label', { class: 'row muted', style: 'cursor:pointer;font-size:12.5px;margin-left:auto' },
        h('input', { type: 'checkbox', checked: UI.autoRefreshPreview, onchange: (e) => { UI.autoRefreshPreview = e.target.checked; APP.rerender(); } }), L('自动刷新', 'Auto-refresh'))),
    hUnhealthy ? h('div', { class: `callout ${retryPaused ? '' : hStatus === 'offline' ? 'err' : 'warn'} drawer-host-banner` },
      h('div', { class: 'stack-xs grow', style: 'min-width:0' },
        h('div', { class: 'row', style: 'gap:6px' },
          span(`tag ${hStatus === 'offline' ? 'soft-red' : 'soft-amber'}`, hostStatusLabel(hStatus)),
          h('strong', { style: 'font-size:12.5px' }, hostDisplayName(hid, s)),
          retryPaused
            ? span('muted', L('已停止重试', 'Auto-retry paused'))
            : (retryCd ? span('muted tabular', retryCd) : null)),
        hRec?.error ? h('span', { style: 'font-size:12px;word-break:break-word' }, hRec.error) : null),
      h('div', { class: 'row', style: 'gap:6px;flex-shrink:0' },
        !retryPaused ? btn(L('停止重试', 'Stop retry'), {
          cls: 'sm ghost',
          busyKey: `pause-host:${hid}`,
          title: L(`停止自动重试主机 ${hostDisplayName(hid, s)}`, `Pause automatic retries for host ${hostDisplayName(hid, s)}`),
          onclick: () => pauseHostRetryApi(hid),
        }) : null,
        btn(L('重连', 'Retry'), {
          cls: 'sm',
          busyKey: `probe-host:${hid}`,
          title: retryPaused
            ? L(`恢复并重新连接主机 ${hostDisplayName(hid, s)}`, `Resume and reconnect host ${hostDisplayName(hid, s)}`)
            : L(`立即重新探测主机 ${hostDisplayName(hid, s)}`, `Probe and reconnect host ${hostDisplayName(hid, s)} now`),
          onclick: () => probeHostApi(hid),
        }))) : null,
    drawerMetricsCard(s.metrics),
    kvGrid([
      [L('所属主机', 'Host'), h('span', { class: 'row', style: 'gap:6px' },
        h('strong', null, hostDisplayName(hid, s)),
        remote && hid !== hostDisplayName(hid, s) ? h('code', null, hid) : null,
        remote ? span('tag', hKind === 'ssh' ? 'SSH' : 'Peer') : null,
        remote ? span(`tag ${hStatus === 'online' ? 'soft-green' : hStatus === 'offline' ? 'soft-red' : 'soft-amber'}`, hostStatusLabel(hStatus)) : null,
        remote && hStatus !== 'offline' ? rttPill(hRec?.rtt_ms, hRec?.sync_ms) : null,
        remote && hStatus !== 'offline' ? syncPill(hRec?.sync_ms) : null,
        hUnhealthy ? btn(L('重连', 'Retry'), { cls: 'xs', busyKey: `probe-host:${hid}`, onclick: () => probeHostApi(hid) }) : null)],
      [L('复用器', 'Multiplexer'), span(muxKindOf(s) === 'zellij' ? 'tag mux-zj' : 'tag', muxKindOf(s))],
      [L('会话 ID', 'Session ID'), h('code', null, s.session_id)],
      s.resume_id ? [L('恢复 ID', 'Resume ID'), h('code', null, s.resume_id)] : null,
      s.state === 'suspended' && s.suspended_at_ms ? [L('休眠时间', 'Suspended at'), ago(s.suspended_at_ms)] : null,
      [L('窗口标识', 'Window label'), s.tmux_label ? h('code', null, s.tmux_label) : null],
      [L('Agent / 模型', 'Agent / Model'), h('div', { class: 'row' }, agentTag(s.agent), modelTag(modelBaseOf(s.model, s.effort) || s.model))],
      [L('思考强度', 'Reasoning effort'), s.effort ? span('tag mono', s.effort) : span('muted', '—')],
      [L('状态', 'State'), statusInd(s.state, L(`${stateText(s.state)}（持续 ${dur(Date.now() - (s.state_since_ms || Date.now()))}）`, `${stateText(s.state)} (${dur(Date.now() - (s.state_since_ms || Date.now()))})`))],
      s.last_error ? [L('最近错误', 'Last error'), span('mono', s.last_error)] : null,
      [L('来源', 'Discovery'), s.discovered_only ? span('tag legacy', L('终端扫描发现', 'Terminal scan')) : span('tag soft-green', L('Hook 实时直连', 'Live hook'))],
      [L('终端位置', 'Terminal target'), s.tmux_target ? h('code', null, `${s.tmux_target} (${paneDisplayId(s) || '—'})`) : h('code', null, paneDisplayId(s) || '—')],
      [L('工作目录', 'Working dir'), h('code', null, s.cwd || '—')],
      [L('Git 根目录', 'Git root'), h('code', null, s.git_root || '—')],
      [L('最近活动', 'Last activity'), ago(s.last_event_ms)],
    ].filter(Boolean)),
    h('div', { class: 'stack-xs' },
      h('div', { class: 'sec-h' }, L(`子会话 / Swarm（${subs.length || s.subagent_count || 0}）`, `Subagents / Swarm (${subs.length || s.subagent_count || 0})`)),
      subs.length ? subs.map((c) => h('button', {
        type: 'button',
        class: 'callout row sub-drawer-btn',
        style: 'justify-content:space-between',
        onclick: () => { closeDrawer(); APP.openSession(c.session_id); },
      },
      h('div', { class: 'row' }, statusInd(c.state), agentTag(c.agent), h('code', null, subagentDisplayName(c))),
      span('muted trunc', c.last_message || L('点击查看对话 →', 'Open transcript →'))))
        : h('div', { class: 'muted', style: 'font-size:13px' }, s.subagent_count ? L(`累计 ${s.subagent_count} 个子会话（当前活跃 ${s.active_subagents || 0}）`, `${s.subagent_count} total subagents (${s.active_subagents || 0} active)`) : L('暂无嵌套子会话', 'No nested subagents yet'))),
    h('div', { class: 'stack-xs' },
      h('div', { class: 'sec-h' }, L('Handoff 交接记录', 'Handoff History')),
      !ho || ho.state === 'loading' ? skeleton(2)
        : ho.state === 'err' ? errorBanner(L(`加载交接失败：${ho.err}`, `Failed to load handoffs: ${ho.err}`), L('重试', 'Retry'), () => loadHandoffs(s.session_id))
        : ho.items.length ? ho.items.map((x) => h('div', { class: 'callout stack-xs' },
          h('div', { class: 'row' }, span('tag', x.intent), span('tag', x.status), h('code', null, x.id), span('muted tabular', clock(x.created_ms))),
          h('div', { style: 'font-weight:500' }, x.summary)))
        : h('div', { class: 'muted', style: 'font-size:13px' }, L('暂无 Handoff 交接记录', 'No handoff records yet'))),
    evs.length ? h('div', { class: 'stack-xs' },
      h('div', { class: 'sec-h' }, L(`近期实时事件（${evs.length}）`, `Recent Events (${evs.length})`)),
      evs.map((e) => h('div', { class: 'row', style: 'font-size:12.5px' }, span('mono dim', clock(e.ts_ms)), span('tag', e.kind), span('muted trunc grow', e.detail?.error || e.detail?.tool_summary || e.detail?.last_message || e.native_event || '')))) : null,
    h('details', null, h('summary', null, L('查看原始 Session JSON', 'Raw Session JSON')), h('pre', { class: 'code-block' }, JSON.stringify(s, null, 2))));
}

function drawerArtifact(id) {
  const a = artifact(id);
  if (a.state === 'loading') return skeleton(6);
  if (a.state === 'err') return errorBanner(L(`工件加载失败：${a.err}`, `Failed to load artifact: ${a.err}`), L('重试', 'Retry'), () => { S.artifacts.delete(id); APP.schedule(); });
  const d = a.data || {};
  const hd = d.header || {};
  let content;
  if (d.kind === 'plan') {
    content = h('div', { class: 'stack' },
      kvGrid([[L('目标', 'Goal'), d.goal], [L('原生策略', 'Native strategy'), d.native_strategy ? span('tag soft-plum', d.native_strategy) : null]]),
      h('div', null, h('div', { class: 'sec-h' }, L('设计方案', 'Design')), mdProse(d.design_md)),
      h('div', null, h('div', { class: 'sec-h' }, L(`任务列表（${(d.tasks || []).length}）`, `Tasks (${(d.tasks || []).length})`)), plainList((d.tasks || []).map((t) => `${t.title}${t.files?.length ? ` (${t.files.join(', ')})` : ''}`))),
      h('div', null, h('div', { class: 'sec-h' }, L('验收标准', 'Acceptance Criteria')), plainList(d.acceptance)),
      h('div', null, h('div', { class: 'sec-h' }, L('测试计划', 'Test Plan')), plainList(d.test_plan)),
      d.risks?.length ? h('div', null, h('div', { class: 'sec-h' }, L('风险点', 'Risks')), plainList(d.risks)) : null);
  } else if (d.kind === 'triage') {
    content = h('div', { class: 'stack' },
      kvGrid([[L('故障现象', 'Symptom'), d.symptom], [L('快筛耗时', 'Triage duration'), d.elapsed_secs != null ? dur(d.elapsed_secs * 1000) : null]]),
      ...(d.hypotheses || []).map((hy, i) => h('div', { class: 'hyp-card' },
        h('div', { class: 'row', style: 'justify-content:space-between' }, span('tag soft-amber', L(`假设 #${i + 1} · 未证实`, `Hypothesis #${i + 1} · Unconfirmed`)), span('tabular muted', pct(hy.confidence))),
        h('div', { style: 'font-weight:600' }, hy.claim),
        evBlocks(hy.evidence_refs),
        kvGrid([[L('确认方法', 'How to verify'), hy.how_to_confirm]]))),
      d.mitigation ? h('div', { class: 'mitig-box' }, h('div', { class: 'sec-h' }, L('临时止血方案', 'Temporary Mitigation')), renderMarkdown(d.mitigation)) : null,
      d.probes?.length ? h('div', null, h('div', { class: 'sec-h' }, L('建议检查命令', 'Suggested Probes')), plainList(d.probes)) : null);
  } else if (d.kind === 'rca') {
    const rc = d.root_cause || {};
    const testCard = (lbl, t) => (t ? h('div', { class: 'callout stack-xs' },
      h('div', { class: 'row', style: 'justify-content:space-between' }, h('strong', null, lbl), statusInd(t.outcome)),
      h('code', null, `$ ${t.cmd}`),
      t.output_tail ? h('pre', { class: 'code-block' }, t.output_tail) : null) : null);
    content = h('div', { class: 'stack' },
      h('div', null, h('div', { class: 'sec-h' }, L('已验证根因', 'Verified Root Cause')), renderMarkdown(rc.claim || '—')),
      rc.trigger_path?.length ? h('div', null, h('div', { class: 'sec-h' }, L('触发路径', 'Trigger Path')), codeRefList(rc.trigger_path)) : null,
      rc.evidence?.length ? h('div', null, h('div', { class: 'sec-h' }, L('证据链', 'Evidence')), evBlocks(rc.evidence)) : null,
      testCard(L('修复前复现', 'Repro Before Fix'), d.repro_before),
      d.fix ? kvGrid([[L('修复分支', 'Fix branch'), h('code', null, d.fix.branch)], ['Commit', d.fix.commit ? h('code', null, short(d.fix.commit, 10)) : null], [L('修改文件', 'Files changed'), d.fix.files?.join(', ')]]) : null,
      testCard(L('修复后验证', 'Verification After Fix'), d.verification_after),
      d.ruled_out?.length ? h('div', null, h('div', { class: 'sec-h' }, L('已排除假设', 'Ruled-out Hypotheses')), d.ruled_out.map((r) => h('div', { class: 'callout' }, h('s', null, r.hypothesis), evBlocks(r.evidence)))) : null);
  } else if (d.kind === 'report') {
    content = h('div', { class: 'stack' }, mdProse(d.summary_md),
      d.changes?.length ? h('div', null, h('div', { class: 'sec-h' }, L('变更摘要', 'Changes')), plainList(d.changes)) : null,
      d.decisions?.length ? h('div', null, h('div', { class: 'sec-h' }, L('关键决策', 'Decisions')), plainList(d.decisions)) : null,
      d.follow_ups?.length ? h('div', null, h('div', { class: 'sec-h' }, L('后续跟进', 'Follow-ups')), plainList(d.follow_ups)) : null);
  } else {
    content = h('pre', { class: 'code-block' }, JSON.stringify(d, null, 2));
  }
  return h('div', { class: 'stack' },
    h('div', { class: 'row' }, span('tag', d.kind || 'artifact'), hd.author ? [agentTag(hd.author.agent), modelTag(hd.author.model)] : null, hd.commit ? span('mono muted', `@${short(hd.commit, 8)}`) : null),
    content,
    h('details', null, h('summary', null, L('查看原始 Artifact JSON', 'Raw Artifact JSON')), h('pre', { class: 'code-block' }, JSON.stringify(d, null, 2))));
}

function drawerFinding(rev, af, dropped) {
  const f = af.finding;
  const g2 = g2Of(af);
  const g3 = g3Of(af);
  const nk = `dfn:${rev.id}:${f.id}`;
  const note = () => draft(nk).trim() || undefined;
  const post = (verb, payload, okMsg) => act(`df:${verb}:${f.id}`, async () => {
    await api('POST', `/api/reviews/${enc(rev.id)}/findings/${enc(f.id)}/${verb}`, payload);
    await refreshReview(rev.id);
  }, okMsg);
  const dgKey = `dfdg:${rev.id}:${f.id}`;
  const dgSel = selectField(dgKey, SEVERITIES.slice(1).map((x) => [x, x]), 'minor', { class: 'inp sm' });
  return h('div', { class: 'stack' },
    dropped ? h('div', { class: 'callout warn' }, L(`该条目已被过滤（${dropReasons(af)}），保留在此供审计与申诉。`, `Filtered (${dropReasons(af)}), preserved here for audit and appeal.`)) : null,
    h('div', { class: 'prose', style: 'font-weight:600;font-size:15px' }, f.claim),
    kvGrid([
      [L('严重级别 / 类别', 'Severity / Category'), h('div', { class: 'row' }, span(`tag ${sevTagCls(effSev(af))}`, effSev(af)), span('tag', f.category), span('tabular muted', L(`置信度 ${pct(f.confidence)}`, `Confidence ${pct(f.confidence)}`)))],
      [L('代码位置', 'Location'), h('code', null, `${f.location?.file}:${f.location?.line_start}-${f.location?.line_end}${f.location?.symbol ? ` · ${f.location.symbol}` : ''}`)],
      [L('验证方式', 'Verification'), span('tag', f.verification_method)],
      [L('前置条件', 'Preconditions'), f.preconditions],
      [L('可观察影响', 'Observable impact'), f.observable_impact || span('muted', L('未提供', 'Not provided'))],
      [L('触发路径', 'Trigger path'), codeRefList(f.trigger_path) || span('muted', L('未提供', 'Not provided'))],
      [L('已检查 Guard', 'Checked guards'), codeRefList(f.checked_guards) || span('muted', L('未提供', 'Not provided'))],
      [L('修复建议', 'Suggested fix'), f.suggested_fix],
    ]),
    h('div', null, h('div', { class: 'sec-h' }, L('G2 证据核实', 'G2 Evidence Verification')), g2 ? statusInd(g2, `G2 ${g2}`, true) : null, evBlocks(f.evidence)),
    af.falsifier ? h('div', { class: 'callout stack-xs' },
      h('div', { class: 'row' }, h('strong', null, L('G3 Falsifier 裁决', 'G3 Falsifier Verdict')), statusInd(g3 || 'pending_falsifier', g3)),
      renderMarkdown(af.falsifier.rationale),
      af.falsifier.counter_evidence?.length ? h('div', null, h('div', { class: 'sec-h' }, L('反证代码', 'Counter-evidence')), evBlocks(af.falsifier.counter_evidence)) : null) : null,
    h('div', { class: 'stack-sm', style: 'border-top:1px solid var(--border);padding-top:14px' },
      field(nk, { class: 'inp sm', placeholder: L('添加反馈或裁决备注（可选）…', 'Optional feedback or verdict note…') }),
      h('div', { class: 'row' },
        btn(L('标记有效', 'Mark Valid'), { cls: 'sm', onclick: () => post('feedback', { label: 'valid', note: note() }, L('已标记有效', 'Marked valid')) }),
        btn(L('标记误报', 'Mark False Positive'), { cls: 'sm', onclick: () => post('feedback', { label: 'false_positive', note: note() }, L('已标记误报', 'Marked false positive')) }),
        btn(L('暂不修复', "Won't Fix"), { cls: 'ghost sm', onclick: () => post('feedback', { label: 'wontfix', note: note() }, L('已标记暂不修复', "Marked won't fix")) })),
      (g3 === 'needs_human' || dropped) ? h('div', { class: 'row' },
        btn(dropped ? L('申诉恢复为 confirmed', 'Reinstate as confirmed') : L('人工确认 confirmed', 'Confirm finding'), { cls: 'primary sm', onclick: () => post('verdict', { verdict: 'confirmed', note: note() }, L('已确认 confirmed', 'Confirmed')) }),
        !dropped ? [dgSel, btn(L('降级', 'Downgrade'), { cls: 'sm', onclick: () => post('verdict', { verdict: 'downgraded', to: draft(dgKey, 'minor'), note: note() }, L('已降级', 'Downgraded')) }),
          btn(L('证伪 disproved', 'Mark disproved'), { cls: 'danger sm', onclick: () => post('verdict', { verdict: 'disproved', note: note() }, L('已标记 disproved', 'Marked disproved')) })] : null) : null));
}

function drawerPipeline(p) {
  return h('div', { class: 'stack' },
    kvGrid([
      ['Pipeline ID', h('code', null, p.id)],
      [L('类型', 'Playbook'), p.playbook === 'debug' ? L('两速查错', 'Two-Speed Debug') : L('功能协作', 'Feature Pipeline')],
      [L('状态', 'Status'), statusInd(p.status)],
      [L('项目路径', 'Project root'), h('code', null, p.project?.root || '—')],
      ['Worktree', typeof p.project?.worktree === 'string' ? p.project.worktree : 'auto'],
      [L('发起渠道', 'Source'), p.source?.via || '—'],
      [L('原始话术', 'Utterance'), p.source?.utterance || '—'],
      [L('创建时间', 'Created'), p.created_ms ? new Date(p.created_ms).toLocaleString(localeTag(), { hour12: false }) : '—'],
    ]),
    h('details', null, h('summary', null, L('查看原始 Pipeline JSON', 'Raw Pipeline JSON')), h('pre', { class: 'code-block' }, JSON.stringify(p, null, 2))));
}

function drawerPaste(sid, att) {
  if (att.kind === 'image') {
    return h('div', { class: 'stack-sm' },
      h('div', { class: 'drawer-controls' },
        att.path ? btn(L('复制图片路径', 'Copy image path'), { cls: 'sm', onclick: (e) => copyText(att.path, e.currentTarget) }) : null,
        btn(L('移除图片', 'Remove image'), { cls: 'ghost sm danger', onclick: () => { removeAttachment(sid, att.id); closeDrawer(); } })),
      att.previewUrl ? h('div', { class: 'drawer-img-wrap' }, h('img', { class: 'drawer-img', src: att.previewUrl, alt: att.title })) : skeleton(4),
      att.path ? kvGrid([
        att.token ? [L('内联占位符', 'Inline token'), h('code', null, att.token)] : null,
        [L('服务端路径', 'Server path'), h('code', null, att.path)],
        [L('大小', 'Size'), fmtBytes(att.bytes)],
      ].filter(Boolean)) : null);
  }
  return h('div', { class: 'stack-sm' },
    h('div', { class: 'drawer-controls' },
      btn(L('复制全文', 'Copy all'), { cls: 'sm', onclick: (e) => copyText(att.content, e.currentTarget) }),
      btn(L('转入输入框', 'Expand inline'), { cls: 'sm', onclick: () => { expandAttachmentToInput(sid, att.id); closeDrawer(); } }),
      btn(L('移除附件', 'Remove attachment'), { cls: 'ghost sm danger', onclick: () => { removeAttachment(sid, att.id); closeDrawer(); } })),
    h('pre', { class: 'code-block', style: 'max-height:none' }, att.content));
}

let lastModalKey = null;
let lastModalCoreSig = '';
let lastModalListSig = '';

function attachModalAutoGrow(ta) {
  if (!ta) return ta;
  ta.classList.add('modal-autogrow-ta');
  const grow = () => {
    if (!ta.isConnected) return;
    const isMob = window.innerWidth <= 768;
    const kbOpen = document.documentElement.classList.contains('kb-open');
    const vvH = window.visualViewport?.height || window.innerHeight || 800;
    const minH = isMob ? 76 : 80;
    const maxH = kbOpen
      ? Math.min(136, Math.max(84, Math.floor(vvH * 0.28)))
      : (isMob ? 180 : 240);
    const atEnd = ta.selectionStart != null && ta.selectionStart === ta.value.length && ta.selectionEnd === ta.value.length;
    if (ta.offsetHeight >= maxH - 1 && ta.scrollHeight > maxH + 2) {
      ta.style.height = `${maxH}px`;
      if (atEnd) ta.scrollTop = ta.scrollHeight;
      return;
    }
    const prevTop = ta.scrollTop;
    ta.style.height = 'auto';
    const nextH = Math.min(maxH, Math.max(minH, ta.scrollHeight));
    ta.style.height = `${nextH}px`;
    if (atEnd && ta.scrollHeight > ta.clientHeight) {
      ta.scrollTop = ta.scrollHeight;
    } else if (prevTop > 0) {
      ta.scrollTop = prevTop;
    }
  };
  ta._autoGrow = grow;
  ta.addEventListener('input', () => {
    grow();
    APP.ensureOverlayFieldVisible?.(ta);
  });
  if (typeof requestAnimationFrame === 'function') requestAnimationFrame(grow);
  return ta;
}

export function renderModal() {
  const d = $('#modal');
  const want = S.authNeeded ? 'auth' : UI.modal;
  if (!want) {
    lastModalKey = null;
    lastModalCoreSig = '';
    lastModalListSig = '';
    if (d.open) d.close();
    return;
  }
  const wasOpen = Boolean(d.open);
  if (!wasOpen) {
    lastModalKey = null;
    lastModalCoreSig = '';
    lastModalListSig = '';
    d.showModal();
  }
  if (want === 'auth') {
    const sig = `auth|${getLang()}`;
    if (wasOpen && lastModalKey === 'auth' && lastModalCoreSig === sig) return;
    lastModalKey = 'auth';
    lastModalCoreSig = sig;
    lastModalListSig = '';
    d.replaceChildren(authModalBody());
  } else if (want === 'spawn') {
    const ctx = buildSpawnModalContext();
    const { coreSig, listSig, projects, sessionCandidates } = computeSpawnModalSigs(ctx);
    if (wasOpen && lastModalKey === 'spawn' && lastModalCoreSig === coreSig) {
      if (lastModalListSig !== listSig) {
        lastModalListSig = listSig;
        const projDl = d.querySelector('#sp-projects');
        if (projDl) projDl.replaceChildren(...projects.map((p) => h('option', { value: p })));
        const sessDl = d.querySelector('#sp-sessions');
        if (sessDl) sessDl.replaceChildren(...sessionCandidates.map((s) => h('option', { value: s })));
      }
      return;
    }
    lastModalKey = 'spawn';
    lastModalCoreSig = coreSig;
    lastModalListSig = listSig;
    d.replaceChildren(spawnModalBody(ctx));
  } else if (want === 'rename-host') {
    const hid = UI.renameHostId || 'local';
    const sig = `rename-host|${getLang()}|${hid}|${hostDisplayName(hid)}|${UI.busy.has(`rename-host:${hid}`) ? 1 : 0}`;
    if (wasOpen && lastModalKey === 'rename-host' && lastModalCoreSig === sig) return;
    lastModalKey = 'rename-host';
    lastModalCoreSig = sig;
    lastModalListSig = '';
    d.replaceChildren(renameHostModalBody());
  } else if (want === 'close-session') {
    lastModalKey = 'close-session';
    lastModalCoreSig = '';
    lastModalListSig = '';
    d.replaceChildren(closeSessionModalBody());
  } else if (want === 'suspend-fresh') {
    lastModalKey = 'suspend-fresh';
    lastModalCoreSig = '';
    lastModalListSig = '';
    d.replaceChildren(suspendFreshModalBody());
  } else if (want === 'usage') {
    lastModalKey = 'usage';
    lastModalCoreSig = '';
    lastModalListSig = '';
    if (S.usageState === 'idle') loadUsage(S.usageDays);
    d.replaceChildren(usageModalBody());
  } else {
    const pv = UI.parsePreviews[`modal:${want}`];
    const sig = `${want}|${getLang()}|${UI.autoConfirm ? 1 : 0}|${UI.busy.has(`mcreate:${want}`) ? 1 : 0}|${JSON.stringify(pv?.data?.resolved || null)}`;
    if (wasOpen && lastModalKey === want && lastModalCoreSig === sig) return;
    lastModalKey = want;
    lastModalCoreSig = sig;
    lastModalListSig = '';
    d.replaceChildren(pipelineModalBody(want));
  }
}

function usageKpiCard(title, totals) {
  const tot = isFin(totals?.total_tokens) ? totals.total_tokens : 0;
  const inp = isFin(totals?.input_tokens) ? totals.input_tokens : 0;
  const out = isFin(totals?.output_tokens) ? totals.output_tokens : 0;
  const cache = isFin(totals?.cache_read_tokens) ? totals.cache_read_tokens : 0;
  const hit = cache > 0 ? cacheHitRate(inp, cache) : null;
  const hitTone = hit != null ? cacheHitTone(hit) : '';
  return h('div', { class: 'usage-kpi-card' },
    h('div', { class: 'usage-kpi-head' },
      h('span', { class: 'usage-kpi-lbl' }, title),
      hit != null
        ? h('span', {
          class: `usage-hit-pill is-${hitTone}`,
          title: L(`缓存命中率：${fmtTokens(cache)} / ${fmtTokens(inp)}`, `Cache hit rate: ${fmtTokens(cache)} / ${fmtTokens(inp)}`),
        }, L(`命中率 ${fmtHitRate(hit)}`, `${fmtHitRate(hit)} hit`))
        : null),
    h('div', { class: 'usage-kpi-val' }, `${fmtTokens(tot)}`),
    h('div', { class: 'usage-kpi-sub' }, L(`输入 ${fmtTokens(inp)} · 输出 ${fmtTokens(out)}`, `In ${fmtTokens(inp)} · Out ${fmtTokens(out)}`)),
    cache > 0 ? h('div', { class: 'usage-kpi-sub' }, L(`缓存命中 ${fmtTokens(cache)}`, `Cache read ${fmtTokens(cache)}`)) : null);
}

const USAGE_AGENT_ORDER = ['claude', 'codex', 'agy'];

function usageModalBody() {
  const days = S.usageDays || 14;
  const data = S.usage;
  const loading = S.usageState === 'loading';
  const rangeBtns = h('div', { class: 'view-seg', role: 'tablist', 'aria-label': L('统计周期', 'Date range') },
    [7, 14, 30].map((d) => h('button', {
      type: 'button',
      role: 'tab',
      class: days === d ? 'on' : '',
      'aria-selected': days === d ? 'true' : 'false',
      onclick: () => loadUsage(d),
    }, L(`${d} 天`, `${d}d`))));

  let content;
  if (!data && S.usageState === 'err') {
    content = errorBanner(
      L(`加载 Token 统计失败：${S.usageErr || ''}`, `Failed to load token usage: ${S.usageErr || ''}`),
      L('重试', 'Retry'),
      () => loadUsage(days),
    );
  } else if (!data) {
    content = skeleton(6);
  } else {
    const daily = Array.isArray(data.daily) ? data.daily : [];
    const maxTot = Math.max(1, ...daily.map((b) => (isFin(b?.total_tokens) ? b.total_tokens : 0)));
    const rangeTot = Math.max(0, data.range_totals?.total_tokens || 0);
    const rangeHit = (data.range_totals?.cache_read_tokens || 0) > 0
      ? cacheHitRate(data.range_totals?.input_tokens, data.range_totals?.cache_read_tokens)
      : null;
    const agentEntries = Object.entries(data.by_agent_range || {})
      .filter(([, tot]) => (tot?.total_tokens || 0) > 0)
      .sort((a, b) => (b[1]?.total_tokens || 0) - (a[1]?.total_tokens || 0));

    const selDay = daily.find((b) => b.date === S.usageSelDate) || daily[daily.length - 1] || null;
    const tickStep = days <= 10 ? 1 : days <= 16 ? 2 : 5;

    const chartBars = daily.map((b, idx) => {
      const tot = isFin(b?.total_tokens) ? b.total_tokens : 0;
      const fillPct = tot > 0 ? Math.max(4, Math.round((tot / maxTot) * 100)) : 0;
      const byAg = b?.by_agent || {};
      const segKeys = [
        ...USAGE_AGENT_ORDER.filter((ag) => (byAg[ag]?.total_tokens || 0) > 0),
        ...Object.keys(byAg).filter((ag) => !USAGE_AGENT_ORDER.includes(ag) && (byAg[ag]?.total_tokens || 0) > 0),
      ];
      const segs = segKeys.map((ag) => {
        const v = byAg[ag]?.total_tokens || 0;
        const cls = USAGE_AGENT_ORDER.includes(ag) ? `a-${ag}` : 'a-other';
        return h('span', { class: `usage-bar-seg ${cls}`, style: `flex-grow:${Math.max(1, v)}` });
      });
      const isLast = idx === daily.length - 1;
      const showTick = idx === 0 || isLast || (idx % tickStep === 0 && (daily.length - 1 - idx) >= Math.ceil(tickStep / 2));
      const agTip = segKeys.map((ag) => `${ag} ${fmtTokens(byAg[ag]?.total_tokens || 0)}`).join(' · ');
      const barHit = (b?.cache_read_tokens || 0) > 0 ? cacheHitRate(b?.input_tokens, b?.cache_read_tokens) : null;
      const hitTip = barHit != null ? L(` · 命中率 ${fmtHitRate(barHit)}`, ` · ${fmtHitRate(barHit)} hit`) : '';
      const tip = `${b.date}: ${fmtTokens(tot)} tokens${hitTip}${agTip ? ` (${agTip})` : ''}`;
      return h('button', {
        type: 'button',
        class: `usage-bar-col${selDay && selDay.date === b.date ? ' is-sel' : ''}`,
        title: tip,
        'aria-label': tip,
        onclick: () => { S.usageSelDate = b.date; APP.rerender(); },
      },
      h('span', { class: 'usage-bar-track' },
        tot > 0 ? h('span', { class: 'usage-bar-fill', style: `height:${fillPct}%` }, segs) : null),
      h('span', { class: 'usage-bar-tick' }, showTick ? b.date.slice(5) : ''));
    });

    const selHit = selDay && (selDay.cache_read_tokens || 0) > 0
      ? cacheHitRate(selDay.input_tokens, selDay.cache_read_tokens)
      : null;
    const selAgText = selDay
      ? Object.entries(selDay.by_agent || {})
        .filter(([, v]) => (v?.total_tokens || 0) > 0)
        .sort((a, b) => (b[1]?.total_tokens || 0) - (a[1]?.total_tokens || 0))
        .map(([ag, v]) => `${ag} ${fmtTokens(v.total_tokens)}`)
        .join(' · ')
      : '';

    const topSessions = Array.isArray(data.top_sessions) ? data.top_sessions : [];

    content = h('div', { class: 'stack' },
      S.usageState === 'err' && S.usageErr
        ? errorBanner(L(`刷新失败：${S.usageErr}`, `Refresh failed: ${S.usageErr}`), L('重试', 'Retry'), () => loadUsage(days))
        : null,
      h('div', { class: 'usage-kpi-grid' },
        usageKpiCard(L('今日消耗', 'Today'), data.today),
        usageKpiCard(L('近 7 日消耗', 'Last 7 days'), data.last_7_days),
        usageKpiCard(L('近 30 日消耗', 'Last 30 days'), data.last_30_days)),
      agentEntries.length
        ? h('div', { class: 'usage-agent-bar' },
          h('span', { class: 'sec-h', style: 'margin:0' }, L(
            `近 ${data.days} 日总计 · ${fmtTokens(rangeTot)}${rangeHit != null ? ` · 缓存命中率 ${fmtHitRate(rangeHit)}` : ''}`,
            `${data.days}d total · ${fmtTokens(rangeTot)}${rangeHit != null ? ` · ${fmtHitRate(rangeHit)} cache hit` : ''}`,
          )),
          h('div', { class: 'usage-agent-pills' },
            agentEntries.map(([ag, tot]) => {
              const v = tot?.total_tokens || 0;
              const share = rangeTot > 0 ? Math.round((v / rangeTot) * 100) : 0;
              const agHit = (tot?.cache_read_tokens || 0) > 0 ? cacheHitRate(tot?.input_tokens, tot?.cache_read_tokens) : null;
              return h('span', {
                class: 'usage-agent-pill',
                title: L(
                  `输入 ${fmtTokens(tot.input_tokens)} · 输出 ${fmtTokens(tot.output_tokens)} · 缓存 ${fmtTokens(tot.cache_read_tokens)}${agHit != null ? ` · 命中率 ${fmtHitRate(agHit)}` : ''}`,
                  `In ${fmtTokens(tot.input_tokens)} · Out ${fmtTokens(tot.output_tokens)} · Cache ${fmtTokens(tot.cache_read_tokens)}${agHit != null ? ` · ${fmtHitRate(agHit)} hit` : ''}`,
                ),
              }, agentTag(ag), h('span', null, `${fmtTokens(v)} (${share}%)`));
            })))
        : null,
      h('div', { class: 'usage-chart-card' },
        h('div', { class: 'row', style: 'justify-content:space-between' },
          h('span', { class: 'sec-h', style: 'margin:0' }, L(`每日消耗趋势（近 ${data.days} 天）`, `Daily usage (last ${data.days} days)`)),
          h('div', { class: 'usage-legend' },
            USAGE_AGENT_ORDER.map((ag) => h('span', { class: 'usage-legend-item' }, h('i', { class: `usage-legend-dot a-${ag}` }), ag)))),
        daily.length ? h('div', { class: 'usage-chart-cols' }, chartBars)
          : h('div', { class: 'muted', style: 'font-size:13px;padding:12px 0' }, L('暂无每日数据', 'No daily data yet')),
        selDay ? h('div', { class: 'usage-day-detail' },
          h('span', { class: 'row', style: 'gap:8px' },
            h('strong', null, selDay.date),
            span('tag', L(`${selDay.session_count || 0} 个会话`, `${selDay.session_count || 0} sessions`))),
          h('span', null,
            L(
              `总计 ${fmtTokens(selDay.total_tokens || 0)}（输入 ${fmtTokens(selDay.input_tokens || 0)} · 输出 ${fmtTokens(selDay.output_tokens || 0)}${(selDay.cache_read_tokens || 0) > 0 ? ` · 缓存 ${fmtTokens(selDay.cache_read_tokens)}${selHit != null ? ` (${fmtHitRate(selHit)})` : ''}` : ''}）${selAgText ? ` · ${selAgText}` : ''}`,
              `Total ${fmtTokens(selDay.total_tokens || 0)} (in ${fmtTokens(selDay.input_tokens || 0)} · out ${fmtTokens(selDay.output_tokens || 0)}${(selDay.cache_read_tokens || 0) > 0 ? ` · cache ${fmtTokens(selDay.cache_read_tokens)}${selHit != null ? ` (${fmtHitRate(selHit)})` : ''}` : ''})${selAgText ? ` · ${selAgText}` : ''}`,
            ))) : null),
      h('div', { class: 'stack-xs' },
        h('div', { class: 'row', style: 'justify-content:space-between' },
          h('span', { class: 'sec-h', style: 'margin:0' }, L(`会话消耗排行（近 ${data.days} 天 Top ${topSessions.length}）`, `Top Sessions (last ${data.days}d)`)),
          h('span', { class: 'muted', style: 'font-size:12px' }, L('点击在线会话可直接跳转', 'Click active sessions to jump'))),
        topSessions.length
          ? h('div', { class: 'usage-sessions-scroll' },
            h('table', { class: 'tbl usage-tbl' },
              h('thead', null,
                h('tr', null,
                  h('th', null, L('会话', 'Session')),
                  h('th', null, L('模型', 'Model')),
                  h('th', { class: 'num' }, L('输入 / 输出', 'In / Out')),
                  h('th', { class: 'num' }, L('缓存命中率', 'Cache Hit')),
                  h('th', { class: 'num' }, L('总消耗', 'Total')),
                  h('th', { class: 'num' }, L('最近活跃', 'Last active')))),
              h('tbody', null,
                topSessions.map((item) => {
                  const live = S.sessions.get(item.session_id);
                  const clickable = Boolean(live);
                  const lbl = live ? sessionLabel(live) : (item.label || short(item.session_id, 14));
                  const isLive = Boolean(item.active || (live && live.state !== 'ended'));
                  const itemHit = cacheHitRate(item.input_tokens, item.cache_read_tokens);
                  const itemHitTone = itemHit != null ? cacheHitTone(itemHit) : 'muted';
                  const openRow = () => {
                    if (!clickable) return;
                    closeModal();
                    APP.openSession(item.session_id);
                  };
                  return h('tr', {
                    class: `usage-sess-row${clickable ? ' is-clickable' : ''}`,
                    tabindex: clickable ? '0' : undefined,
                    role: clickable ? 'button' : undefined,
                    title: clickable ? L(`跳转到会话 ${lbl}`, `Open session ${lbl}`) : item.session_id,
                    onclick: clickable ? openRow : undefined,
                    onkeydown: clickable ? (e) => {
                      if (e.key === 'Enter' || e.key === ' ') { e.preventDefault(); openRow(); }
                    } : undefined,
                  },
                  h('td', null,
                    h('div', { class: 'row', style: 'gap:6px;flex-wrap:nowrap;min-width:0' },
                      agentTag(item.agent),
                      h('span', { class: 'trunc', style: 'max-width:150px;font-weight:500', title: item.session_id }, lbl),
                      isLive ? span('tag soft-green', L('在线', 'Live')) : null)),
                  h('td', null, item.model ? modelTag(item.model) : span('muted', '—')),
                  h('td', {
                    class: 'num mono',
                    title: item.cache_read_tokens > 0
                      ? L(`缓存命中 ${fmtTokens(item.cache_read_tokens)}`, `Cache read ${fmtTokens(item.cache_read_tokens)}`)
                      : undefined,
                  }, `${fmtTokens(item.input_tokens)} / ${fmtTokens(item.output_tokens)}`),
                  h('td', {
                    class: `num mono usage-hit-cell is-${itemHitTone}`,
                    title: itemHit != null
                      ? L(`缓存命中 ${fmtTokens(item.cache_read_tokens || 0)} / 输入 ${fmtTokens(item.input_tokens || 0)}`, `Cache read ${fmtTokens(item.cache_read_tokens || 0)} / In ${fmtTokens(item.input_tokens || 0)}`)
                      : undefined,
                  }, itemHit != null ? fmtHitRate(itemHit) : '—'),
                  h('td', { class: 'num mono' }, h('strong', null, fmtTokens(item.total_tokens))),
                  h('td', { class: 'num muted' }, item.last_active_ms ? ago(item.last_active_ms) : '—'));
                }))))
          : emptyState(L('暂无会话用量记录', 'No session token usage recorded yet'), L('随后台扫描及对话产生的新回合会自动累计到此处。', 'Historical transcripts and live turns will appear here automatically.'))));
  }

  return h('div', { class: 'modal-shell usage-modal' },
    h('div', { class: 'sheet-handle', 'aria-hidden': 'true' }),
    h('div', { class: 'modal-head' },
      h('div', { class: 'stack-xs' },
        h('h3', null, L('Token 消耗统计', 'Token Usage')),
        data?.tz_offset ? h('span', { class: 'muted mono', style: 'font-size:11.5px' }, L(`统计时区 UTC${data.tz_offset} · 按本地日历切分`, `Timezone UTC${data.tz_offset} · local calendar day`)) : null),
      h('div', { class: 'usage-head-acts' },
        rangeBtns,
        btn(loading ? L('刷新中…', 'Refreshing…') : L('刷新', 'Refresh'), {
          cls: 'sm',
          disabled: loading,
          onclick: () => loadUsage(days),
        }),
        btn(L('关闭', 'Close'), { cls: 'ghost sm', onclick: closeModal }))),
    h('div', { class: 'modal-body stack' }, content),
    h('div', { class: 'modal-foot' },
      data?.updated_ms ? h('span', { class: 'muted tabular', style: 'font-size:12px;margin-right:auto' }, L(`更新于 ${clock(data.updated_ms)}`, `Updated ${clock(data.updated_ms)}`)) : null,
      btn(L('关闭', 'Close'), { cls: 'primary', onclick: closeModal })));
}

function suspendFreshModalBody() {
  const sid = UI.suspendFreshSessionId;
  const s = sid ? S.sessions.get(sid) : null;
  if (!s || !canSuspendSession(s)) {
    return h('div', { class: 'modal-shell' },
      h('div', { class: 'sheet-handle', 'aria-hidden': 'true' }),
      h('div', { class: 'modal-head' },
        h('h3', null, L('休眠确认', 'Confirm Suspend')),
        btn(L('关闭', 'Close'), { cls: 'ghost sm', onclick: closeModal })),
      h('div', { class: 'modal-body stack' },
        h('p', { class: 'muted' }, L('该会话当前状态已变更，无法执行休眠。', 'This session can no longer be suspended.'))),
      h('div', { class: 'modal-foot' },
        btn(L('知道了', 'Close'), { cls: 'primary', onclick: closeModal })));
  }
  const label = sessionLabel(s);
  const paneBadge = paneDisplayId(s);
  const hid = hostOf(s);
  return h('div', { class: 'modal-shell' },
    h('div', { class: 'sheet-handle', 'aria-hidden': 'true' }),
    h('div', { class: 'modal-head' },
      h('h3', null, L('以新会话模式休眠？', 'Suspend Without Resume ID?')),
      btn(L('关闭', 'Close'), { cls: 'ghost sm', onclick: closeModal })),
    h('div', { class: 'modal-body stack' },
      h('div', { class: 'close-target-card' },
        h('div', { class: 'close-target-head' },
          h('div', { class: 'row', style: 'gap:7px' },
            h('i', { class: `dot dot-${s.state}`, title: stateText(s.state) }),
            agentTag(s.agent),
            h('span', { class: 'close-target-name' }, label)),
          statusInd(s.state, stateText(s.state))),
        h('div', { class: 'close-target-meta' },
          span('muted', `${L('主机', 'Host')}: ${hostDisplayName(hid, s)}`),
          s.tmux_target ? [span('dim', '·'), h('code', null, s.tmux_target)] : null,
          paneBadge ? [span('dim', '·'), h('code', null, paneBadge)] : null)),
      h('div', { class: 'callout warn' },
        L(
          '当前未识别到该窗口的可恢复对话 ID（常见于尚未发出任何消息的空白启动窗口，或暂未产生对话日志的远端终端）。',
          'No resumable conversation ID is currently associated with this pane (typically a fresh blank prompt with no user turns yet, or an unindexed remote session).',
        )),
      h('p', { class: 'muted', style: 'font-size:13.5px;line-height:1.6' },
        L(
          '如果现在休眠：后台仍会结束当前 Agent 进程以释放 CPU/内存并保留终端窗格；但下次点击「唤醒」或在终端按 Enter 时，将在同一目录重开一个全新的空白会话（不会带回之前仅留在屏幕上的输出）。',
          'If you continue: the agent process will exit to reclaim CPU/RAM and keep this pane open, and waking it later will launch a fresh interactive session in the same directory.',
        ))),
    h('div', { class: 'modal-foot' },
      btn(L('取消', 'Cancel'), { cls: 'ghost', onclick: closeModal }),
      btn(L('仍要休眠（释放进程）', 'Suspend Anyway'), {
        cls: 'primary',
        busyKey: `suspend:${s.session_id}`,
        onclick: () => suspendSessionApi(s.session_id, { allowFresh: true }),
      })));
}

function closeSessionModalBody() {
  const sid = UI.closeSessionId;
  const s = sid ? S.sessions.get(sid) : null;
  if (!s || !canCloseSessionWindow(s)) {
    return h('div', { class: 'modal-shell' },
      h('div', { class: 'sheet-handle', 'aria-hidden': 'true' }),
      h('div', { class: 'modal-head' },
        h('h3', null, L('关闭窗口', 'Close Window')),
        btn(L('关闭', 'Close'), { cls: 'ghost sm', onclick: closeModal })),
      h('div', { class: 'modal-body stack' },
        h('p', { class: 'muted' }, L('该会话已结束或当前无法关闭。', 'This session has already ended or can no longer be closed.'))),
      h('div', { class: 'modal-foot' },
        btn(L('知道了', 'Close'), { cls: 'primary', onclick: closeModal })));
  }
  const siblings = siblingSplitPanesOf(s);
  const isSplit = siblings.length > 0;
  const label = sessionLabel(s);
  const paneBadge = paneDisplayId(s);
  const mux = muxKindOf(s);
  const hid = hostOf(s);
  const busyKey = `close-session:${s.session_id}`;
  const title = isSplit ? L('关闭窗格', 'Close Pane') : L('关闭窗口', 'Close Window');
  const confirmLabel = isSplit ? L('确认关闭窗格', 'Close Pane') : L('确认关闭', 'Close Window');
  const siblingPreview = siblings
    .slice(0, 3)
    .map((x) => (x.tmux_target ? `${sessionLabel(x)} (${x.tmux_target})` : sessionLabel(x)))
    .join(', ')
    + (siblings.length > 3 ? ` +${siblings.length - 3}` : '');

  return h('div', { class: 'modal-shell' },
    h('div', { class: 'sheet-handle', 'aria-hidden': 'true' }),
    h('div', { class: 'modal-head' },
      h('h3', null, title),
      btn(L('关闭', 'Close'), { cls: 'ghost sm', onclick: closeModal })),
    h('div', { class: 'modal-body stack' },
      h('div', { class: 'close-target-card' },
        h('div', { class: 'close-target-head' },
          h('div', { class: 'row', style: 'gap:7px' },
            h('i', { class: `dot dot-${s.state}`, title: stateText(s.state) }),
            agentTag(s.agent),
            h('span', { class: 'close-target-name' }, label)),
          statusInd(s.state, stateText(s.state))),
        h('div', { class: 'close-target-meta' },
          span('muted', `${L('主机', 'Host')}: ${hostDisplayName(hid, s)}`),
          s.tmux_target ? [span('dim', '·'), h('code', null, s.tmux_target)] : null,
          paneBadge ? [span('dim', '·'), h('code', null, paneBadge)] : null)),
      isSplit
        ? h('div', { class: 'close-split-note' },
          h('span', null, L(
            `此操作将终止并仅关闭当前窗格；同一 tmux 窗口内的其余 ${siblings.length} 个分屏窗格将保持运行、不受影响。`,
            `This will terminate and close only this pane. The other ${siblings.length} sibling split ${siblings.length === 1 ? 'pane' : 'panes'} in the same tmux window will remain untouched.`,
          )),
          siblingPreview ? h('span', { class: 'close-split-list' }, `${siblings.length === 1 ? L('保留的分屏：', 'Untouched split pane: ') : L('保留的分屏：', 'Untouched split panes: ')}${siblingPreview}`) : null)
        : h('p', { class: 'muted', style: 'font-size:13.5px;line-height:1.6' },
          mux === 'zellij'
            ? L(
              '此操作将终止该会话中的进程并关闭对应的 Zellij 窗格，其他标签页与窗口不受影响。',
              'This will terminate the running process and close the Zellij pane. Other tabs and windows remain untouched.',
            )
            : L(
              '此操作将终止该会话中的进程并关闭对应的 tmux 窗口，所属 tmux 会话中的其他窗口不受影响。',
              'This will terminate the running process and close the tmux window. Other windows in the tmux session remain untouched.',
            )),
      s.state === 'working'
        ? h('div', { class: 'callout warn' }, L(
          '该会话当前正在运行中，关闭会中断正在执行的任务。',
          'This session is currently working; closing it will interrupt the active run.',
        ))
        : null),
    h('div', { class: 'modal-foot' },
      btn(L('取消', 'Cancel'), { cls: 'ghost', onclick: closeModal }),
      btn(confirmLabel, {
        cls: 'danger-primary',
        busyKey,
        onclick: () => closeSessionApi(s.session_id, { isSplitPane: isSplit }),
      })));
}

function renameHostModalBody() {
  const hostId = UI.renameHostId || 'local';
  const curName = hostDisplayName(hostId);
  const submit = () => {
    const next = draft('host:rename', curName).trim();
    if (!next) { toast(L('主机名称不能为空', 'Host name cannot be empty'), 'warn'); return; }
    renameHostApi(hostId, next);
  };
  const inp = field('host:rename', {
    class: 'inp',
    placeholder: L('输入主机显示名称（如 腾讯云 GPU / 办公 Mac）…', 'Enter host display name (e.g. Cloud GPU / Office Mac)…'),
    autocomplete: 'off',
  }, curName);
  inp.addEventListener('keydown', (e) => { if (e.key === 'Enter' && !e.isComposing) { e.preventDefault(); submit(); } });
  return h('div', { class: 'modal-shell' },
    h('div', { class: 'sheet-handle', 'aria-hidden': 'true' }),
    h('div', { class: 'modal-head' }, h('h3', null, L('重命名主机', 'Rename Host')), btn(L('关闭', 'Close'), { cls: 'ghost sm', onclick: closeModal })),
    h('div', { class: 'modal-body stack' },
      hostId === 'local'
        ? h('p', { class: 'muted' },
          L(
            '修改此电脑在控制台与通知中的显示名称（如 wujie-mac / 办公 Mac），不影响现有会话连接。',
            'Change how this computer is labeled across the console and notifications (e.g. wujie-mac / Office Mac); active sessions stay connected.',
          ))
        : h('p', { class: 'muted' },
          L('仅修改显示名称，不可变主机 ID ', 'Only the display name changes; immutable host ID '),
          h('code', null, hostId),
          L(' 与面板前缀 ', ' and pane prefix '),
          h('code', null, `${hostId}:%...`),
          L(' 保持稳定。', ' stay unchanged.')),
      h('div', null, h('label', { class: 'lbl' }, L('主机显示名称', 'Host display name')), inp)),
    h('div', { class: 'modal-foot' },
      btn(L('取消', 'Cancel'), { cls: 'ghost', onclick: closeModal }),
      btn(L('保存名称', 'Save Name'), { cls: 'primary', busyKey: `rename-host:${hostId}`, onclick: submit })));
}

function buildSpawnModalContext() {
  const hasMultiHosts = S.hosts.size > 1;
  const fallbackHost = hasMultiHosts && S.hosts.has(hostOf(currentSession())) ? hostOf(currentSession()) : 'local';
  const rawHost = draft('sp:host', fallbackHost);
  const targetHost = hasMultiHosts && S.hosts.has(rawHost) ? rawHost : 'local';
  const isRemote = isRemoteHost(targetHost);
  const targetHostRec = hostRecord(targetHost);
  const targetHostStatus = targetHostRec?.status || 'online';
  const targetOffline = isRemote && targetHostStatus === 'offline';
  const targetDegraded = isRemote && targetHostStatus === 'degraded';
  const projects = knownProjects(targetHost);
  const prefs = loadSpawnPrefs();
  const agent = draft('sp:agent', prefs.last_agent || 'claude');
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
  const cachedHostSessions = S.tmuxSessionsByHost.get(targetHost)
    || (!isRemote ? S.tmuxSessions : null);
  const activeSessionGroups = (cachedHostSessions && cachedHostSessions.length)
    ? cachedHostSessions
    : [...S.sessions.values()]
      .filter((s) => s.state !== 'ended' && isTmuxSession(s) && hostOf(s) === targetHost)
      .map((s) => tmuxGroupOf(s))
      .filter((g) => g && g !== NON_TMUX_GROUP && g !== 'tmux');
  const sessionCandidates = [...new Set(['agent-session', ...activeSessionGroups])];
  const loadingSessions = isTmuxSessionsLoading(targetHost);
  return {
    hasMultiHosts,
    targetHost,
    isRemote,
    targetHostRec,
    targetHostStatus,
    targetOffline,
    targetDegraded,
    projects,
    agent,
    mInfo,
    selKey,
    effKey,
    curSel,
    effs,
    fixedEffort,
    curEff,
    sessionCandidates,
    loadingSessions,
  };
}

function computeSpawnModalSigs(ctx) {
  const hostMetaSig = ctx.hasMultiHosts
    ? sortedHosts().map((hr) => `${hr.id}:${hostDisplayName(hr.id)}:${hr.status || 'online'}:${hr.error || ''}`).join('|')
    : 'single';
  const missingSig = UI.spawnMissingDir ? `${UI.spawnMissingDir.hostId}:${UI.spawnMissingDir.path}` : '';
  const coreSig = [
    getLang(),
    ctx.targetHost,
    ctx.agent,
    ctx.curSel,
    ctx.fixedEffort ? 'fixed' : ctx.curEff,
    UI.drafts.get('sp:createDir') === '1' ? 1 : 0,
    ctx.loadingSessions ? 1 : 0,
    UI.busy.has('spawn-sess') ? 1 : 0,
    UI.busy.has(`probe-host:${ctx.targetHost}`) ? 1 : 0,
    S.models ? 1 : 0,
    missingSig,
    UI.spawnErr || '',
    hostMetaSig,
  ].join('\n');
  const listSig = `${ctx.projects.join('\u0001')}\u0002${ctx.sessionCandidates.join('\u0001')}`;
  return {
    coreSig,
    listSig,
    projects: ctx.projects,
    sessionCandidates: ctx.sessionCandidates,
  };
}

function spawnModalBody(ctx = buildSpawnModalContext()) {
  const {
    hasMultiHosts,
    targetHost,
    targetHostRec,
    targetOffline,
    targetDegraded,
    projects,
    agent,
    mInfo,
    selKey,
    effKey,
    curSel,
    fixedEffort,
    curEff,
    sessionCandidates,
    loadingSessions,
  } = ctx;
  const curDir = '';
  const modelOpts = [...mInfo.models.map((m) => [m.id, m.label]), ['__custom__', L('自定义模型…', 'Custom model…')]];
  const effortOpts = fixedEffort ? [['', L('固定配置', 'Fixed')]] : ctx.effs.map((e) => [e, effortLabel(e)]);
  const dlId = 'sp-projects';
  const sessDlId = 'sp-sessions';
  const persistCurrent = (ag, mSel, eSel) => {
    const modelVal = mSel === '__custom__' ? draft('sp:modelCustom').trim() : String(mSel || '').trim();
    saveSpawnPrefs(ag, { model: modelVal || undefined, effort: eSel || '' });
  };
  const create = async (forceCreateDir = false) => {
    const busyKey = 'spawn-sess';
    if (UI.busy.has(busyKey)) return undefined;
    const chosenHost = S.hosts.size > 1 ? draft('sp:host', targetHost) : 'local';
    const effHost = chosenHost && S.hosts.has(chosenHost) ? chosenHost : 'local';
    const remoteHost = isRemoteHost(effHost);
    if (remoteHost && hostRecord(effHost)?.status === 'offline') {
      const msg = L(`目标主机「${hostDisplayName(effHost)}」当前离线，请先重连`, `Target host "${hostDisplayName(effHost)}" is offline — reconnect first`);
      UI.spawnErr = msg;
      UI.spawnMissingDir = null;
      toast(msg, 'err');
      APP.rerender();
      return undefined;
    }
    const wantCreateDir = Boolean(forceCreateDir || UI.drafts.get('sp:createDir') === '1');
    UI.spawnErr = '';
    UI.spawnMissingDir = null;
    UI.busy.add(busyKey);
    APP.schedule();
    try {
      const ag = draft('sp:agent', 'claude');
      const info = modelsForAgent(ag);
      const picked = UI.drafts.get(`sp:modelSel:${ag}`) || info.default;
      const model = picked === '__custom__' ? draft('sp:modelCustom').trim() : String(picked || '').trim();
      const availEffs = effortsForModel(info, picked);
      const pickedEff = availEffs.length ? (UI.drafts.get(`sp:effort:${ag}`) || info.default_effort || availEffs[0]) : '';
      const proj = draft('sp:dir', curDir).trim();
      const tmuxSess = draft('sp:session').trim();
      const payload = {
        agent: ag,
        host_id: remoteHost ? effHost : undefined,
        project_root: proj || undefined,
        tmux_session: tmuxSess || undefined,
        model: model || undefined,
        effort: pickedEff || undefined,
        name: draft('sp:name').trim() || undefined,
        prompt: draft('sp:prompt').trim() || undefined,
        create_dir: wantCreateDir || undefined,
      };
      const r = await api('POST', '/api/sessions/spawn', payload);
      saveSpawnPrefs(ag, { model: model || undefined, effort: pickedEff || '' });
      ['sp:dir', 'sp:createDir', 'sp:session', 'sp:name', 'sp:prompt', 'sp:host'].forEach((k) => UI.drafts.delete(k));
      UI.spawnMissingDir = null;
      UI.spawnErr = '';
      UI.modal = null;
      await loadAll();
      if (r?.session_id) APP.openSession(r.session_id);
      const tgt = r?.tmux_target || r?.pane_id || '';
      const hid = r?.host_id || (remoteHost ? effHost : 'local');
      toast(hid && hid !== 'local'
        ? L(`已在 ${hostDisplayName(hid)} 新建会话 ${tgt}`, `Created session ${tgt} on ${hostDisplayName(hid)}`)
        : L(`已在 tmux 新建会话 ${tgt}`, `Created tmux session ${tgt}`), 'ok');
      return r;
    } catch (e) {
      const m = !wantCreateDir && e instanceof ApiError && e.status === 400
        ? (e.message || '').match(/working directory does not exist(?: on [^:]+)?:\s*(.+)$/i)
        : null;
      if (m) {
        UI.spawnMissingDir = { path: m[1].trim(), hostId: effHost };
        UI.spawnErr = '';
        APP.rerender();
        return undefined;
      }
      const msg = errText(e);
      UI.spawnErr = msg;
      UI.spawnMissingDir = null;
      toast(msg, 'err');
      APP.rerender();
      return undefined;
    } finally {
      UI.busy.delete(busyKey);
      APP.schedule();
    }
  };
  const modelSelect = selectField(selKey, modelOpts, curSel, { class: 'inp', style: 'width:100%' });
  modelSelect.addEventListener('change', () => {
    const nextModel = modelSelect.value;
    const nextEffs = effortsForModel(mInfo, nextModel);
    let nextEff = UI.drafts.get(effKey) || mInfo.default_effort;
    if (!nextEffs.length) nextEff = '';
    else if (!nextEffs.includes(nextEff)) nextEff = nextEffs.includes(mInfo.default_effort) ? mInfo.default_effort : nextEffs[0];
    UI.drafts.set(effKey, nextEff);
    persistCurrent(agent, nextModel, nextEff);
    APP.rerender();
  });
  const effortSelect = selectField(effKey, effortOpts, fixedEffort ? '' : curEff, {
    class: 'inp', style: 'width:100%', disabled: fixedEffort,
  });
  effortSelect.addEventListener('change', () => { persistCurrent(agent, curSel, effortSelect.value); });
  const customModelInp = curSel === '__custom__' ? field('sp:modelCustom', { class: 'inp sm mono', placeholder: L('输入自定义模型 ID…', 'Enter custom model ID…'), autocomplete: 'off' }) : null;
  if (customModelInp) customModelInp.addEventListener('input', () => persistCurrent(agent, '__custom__', fixedEffort ? '' : curEff));
  const hostSelector = hasMultiHosts ? h('div', { class: 'stack-xs' },
    h('label', { class: 'lbl', style: 'margin:0' }, L('目标主机', 'Target Host')),
    h('div', { class: 'chips sp-host-chips' }, sortedHosts().map((hr) => {
      const st = hr.status || 'online';
      const selected = targetHost === hr.id;
      const rttLabel = hr.id !== 'local' && st !== 'offline' ? fmtRtt(hr.rtt_ms) : '';
      const syncLabel = hr.id !== 'local' && st !== 'offline' && isFin(hr.sync_ms) && hr.sync_ms >= 0
        ? L(`同步 ${fmtRtt(hr.sync_ms)}`, `Sync ${fmtRtt(hr.sync_ms)}`)
        : '';
      const tip = [
        `${hostDisplayName(hr.id)} (${hr.id})`,
        hostStatusLabel(st),
        rttLabel,
        syncLabel,
      ].filter(Boolean).join(' · ');
      return h('button', {
        type: 'button',
        class: `chip sp-host-chip${selected ? ' on' : ''}${st === 'offline' ? ' is-offline' : st === 'degraded' ? ' is-degraded' : ''}`,
        'data-sp-host': hr.id,
        title: tip,
        onclick: () => {
          if (hr.id === targetHost) return;
          const prevHost = targetHost;
          const curDirVal = draft('sp:dir').trim();
          if (curDirVal && knownProjects(prevHost).includes(curDirVal)) {
            UI.drafts.delete('sp:dir');
          }
          UI.drafts.set('sp:host', hr.id);
          UI.spawnMissingDir = null;
          UI.spawnErr = '';
          loadTmuxSessions(hr.id);
          APP.rerender();
        },
      },
      h('i', { class: `dot dot-${st}` }),
      h('span', null, hostDisplayName(hr.id)),
      rttLabel ? span('sp-host-rtt tabular', rttLabel) : null,
      st === 'offline' ? span('sp-host-rtt', L('离线', 'Offline')) : null,
      st === 'degraded' && !rttLabel ? span('sp-host-rtt', L('降级', 'Degraded')) : null);
    })),
    targetOffline ? h('div', { class: 'callout err sp-host-warn' },
      h('span', { class: 'grow' }, L(
        `主机「${hostDisplayName(targetHost)}」当前离线${targetHostRec?.error ? `（${targetHostRec.error}）` : ''}，请先重连后再新建会话。`,
        `Host "${hostDisplayName(targetHost)}" is offline${targetHostRec?.error ? ` (${targetHostRec.error})` : ''} — reconnect before creating a session.`,
      )),
      btn(L('重连', 'Retry'), {
        cls: 'xs',
        busyKey: `probe-host:${targetHost}`,
        onclick: async () => {
          const r = await probeHostApi(targetHost);
          if (r && r.status !== 'offline') loadTmuxSessions(targetHost);
        },
      })) : null,
    targetDegraded ? h('div', { class: 'callout warn sp-host-warn' },
      h('span', { class: 'grow' }, L(
        `主机「${hostDisplayName(targetHost)}」连接降级${targetHostRec?.error ? `（${targetHostRec.error}）` : ''}，新建会话可能较慢。`,
        `Host "${hostDisplayName(targetHost)}" is degraded${targetHostRec?.error ? ` (${targetHostRec.error})` : ''} — spawning may take longer.`,
      )),
      btn(L('重连', 'Retry'), {
        cls: 'xs',
        busyKey: `probe-host:${targetHost}`,
        onclick: async () => {
          const r = await probeHostApi(targetHost);
          if (r && r.status !== 'offline') loadTmuxSessions(targetHost);
        },
      })) : null) : null;
  const dirLabel = hasMultiHosts
    ? L(`工作目录（${hostDisplayName(targetHost)}，留空默认 ~/.agent_session）`, `Working directory (${hostDisplayName(targetHost)}; defaults to ~/.agent_session)`)
    : L('工作目录（留空默认 ~/.agent_session）', 'Working directory (defaults to ~/.agent_session)');
  const dirPlaceholder = L('留空默认 ~/.agent_session', 'Defaults to ~/.agent_session');
  const sessPlaceholder = loadingSessions
    ? L('正在获取远端 tmux 会话…', 'Loading remote tmux sessions…')
    : L('默认 agent-session（自动创建）', 'Default: agent-session (auto-created)');
  const dirInp = field('sp:dir', { class: 'inp mono', list: dlId, placeholder: dirPlaceholder, autocomplete: 'off' }, curDir);
  dirInp.addEventListener('input', () => {
    if (UI.spawnMissingDir || UI.spawnErr) {
      UI.spawnMissingDir = null;
      UI.spawnErr = '';
      APP.rerender();
    }
  });
  const autoCreateDir = UI.drafts.get('sp:createDir') === '1';
  const createDirChk = h('label', { class: 'sp-create-dir-opt' },
    h('input', {
      type: 'checkbox',
      checked: autoCreateDir,
      onchange: (e) => {
        if (e.target.checked) UI.drafts.set('sp:createDir', '1');
        else UI.drafts.delete('sp:createDir');
        UI.spawnMissingDir = null;
        UI.spawnErr = '';
        APP.rerender();
      },
    }),
    L('目录不存在时自动创建', 'Auto-create directory if missing'));
  const missingDirBanner = UI.spawnMissingDir
    ? h('div', { class: 'callout warn sp-missing-dir' },
      h('div', { class: 'sp-missing-dir-body' },
        h('div', null, L(
          `工作目录尚不存在${UI.spawnMissingDir.hostId !== 'local' ? `（${hostDisplayName(UI.spawnMissingDir.hostId)}）` : ''}：`,
          `Working directory does not exist${UI.spawnMissingDir.hostId !== 'local' ? ` (on ${hostDisplayName(UI.spawnMissingDir.hostId)})` : ''}:`,
        )),
        h('code', null, UI.spawnMissingDir.path)),
      btn(L('创建目录并启动', 'Create & Launch'), {
        cls: 'sm primary',
        busyKey: 'spawn-sess',
        disabled: targetOffline,
        onclick: () => create(true),
      }))
    : null;
  const spawnErrBanner = UI.spawnErr
    ? h('div', { class: 'callout err', style: 'margin-top:8px;padding:9px 12px;font-size:12.5px' },
      h('span', { style: 'word-break:break-word' }, UI.spawnErr))
    : null;
  return h('div', { class: 'modal-shell' },
    h('div', { class: 'sheet-handle', 'aria-hidden': 'true' }),
    h('div', { class: 'modal-head' }, h('h3', null, L('新建交互会话', 'New Interactive Session')), btn(L('关闭', 'Close'), { cls: 'ghost sm', onclick: closeModal })),
    h('div', { class: 'modal-body stack' },
      h('datalist', { id: dlId }, projects.map((p) => h('option', { value: p }))),
      h('datalist', { id: sessDlId }, sessionCandidates.map((s) => h('option', { value: s }))),
      hostSelector,
      h('div', null,
        h('label', { class: 'lbl' }, L('选择 Agent', 'Agent')),
        h('div', { class: 'chips' }, AGENTS.map((a) => h('button', {
          type: 'button',
          class: `chip${agent === a ? ' on' : ''}`,
          onclick: () => {
            UI.drafts.set('sp:agent', a);
            hydrateSpawnAgent(a);
            const nextModel = UI.drafts.get(`sp:modelSel:${a}`);
            const nextEff = UI.drafts.get(`sp:effort:${a}`);
            persistCurrent(a, nextModel, nextEff);
            APP.rerender();
          },
        }, a)))),
      h('div', { class: 'opt-grid' },
        h('div', { class: 'stack-xs' }, h('label', { class: 'lbl', style: 'margin:0' }, L('模型', 'Model')), modelSelect, customModelInp),
        h('div', { class: 'stack-xs' }, h('label', { class: 'lbl', style: 'margin:0' }, L('思考强度', 'Effort')), effortSelect)),
      h('div', null,
        h('label', { class: 'lbl' }, dirLabel),
        dirInp,
        createDirChk,
        missingDirBanner,
        spawnErrBanner),
      h('div', { class: 'opt-grid' },
        h('div', null, h('label', { class: 'lbl' }, L('tmux 会话（Session，选填）', 'tmux Session (optional)')),
          field('sp:session', { class: 'inp mono', list: sessDlId, placeholder: sessPlaceholder, autocomplete: 'off' })),
        h('div', null, h('label', { class: 'lbl' }, L('tmux 窗口名（Window，选填）', 'tmux Window (optional)')),
          field('sp:name', { class: 'inp mono', placeholder: L(`留空顺延命名（如 ${agent}-1）`, `Auto-numbered if blank (e.g. ${agent}-1)`), autocomplete: 'off' }))),
      h('div', null, h('label', { class: 'lbl' }, L('首条消息（选填）', 'Initial prompt (optional)')),
        attachModalAutoGrow(field('sp:prompt', { tag: 'textarea', rows: 3, class: 'inp modal-autogrow-ta', placeholder: L('留空则仅打开干净的交互式会话…', 'Leave blank to open a fresh interactive session…') })))),
    h('div', { class: 'modal-foot' },
      btn(L('取消', 'Cancel'), { cls: 'ghost', onclick: closeModal }),
      btn(L('新建交互会话', 'Create Session'), {
        cls: 'primary',
        busyKey: 'spawn-sess',
        disabled: targetOffline,
        title: targetOffline ? L('请先重连离线主机后再新建会话', 'Reconnect the offline host before creating a session') : undefined,
        onclick: () => create(false),
      })));
}

function pipelineModalBody(pb) {
  const isDebug = pb === 'debug';
  const scope = `modal:${pb}`;
  const goalKey = `${scope}:goal`;
  const bindKey = `${scope}:bind`;
  const projKey = `${scope}:proj`;
  const curDir = currentSession()?.git_root || currentSession()?.cwd || knownProjects()[0] || '';
  const pv = UI.parsePreviews[scope];
  const bindInp = field(bindKey, { class: 'inp', placeholder: isDebug ? L('选填：如 快速排查用 agy，深挖根因用 codex', 'Optional: e.g. triage with agy, deep RCA with codex') : L('选填：如 计划用 claude，实现用 codex，Review 用 agy', 'Optional: e.g. plan with claude, implement with codex, review with agy') });
  bindInp.addEventListener('input', () => queueParse(scope, pb, projKey, bindKey, curDir));
  const submit = () => act(`mcreate:${pb}`, async () => {
    const goal = draft(goalKey).trim();
    const utt = draft(bindKey).trim();
    const proj = draft(projKey, curDir).trim();
    const combined = [goal, utt].filter(Boolean).join(L('；', '; '));
    if (!combined) throw new Error(isDebug ? L('请描述故障现象', 'Please describe the symptom') : L('请描述需求目标', 'Please describe the feature goal'));
    const parsed = pv?.data && pv.text === utt ? pv.data : null;
    let p = isDebug
      ? await api('POST', '/api/debug', { project_root: proj, symptom: goal || combined, utterance: utt || combined, auto_confirm: UI.autoConfirm })
      : await api('POST', '/api/pipelines', { req: { playbook: 'feature', project_root: proj, goal: goal || combined, bindings: parsed?.bindings || {}, source: { via: 'web', utterance: combined } }, auto_confirm: UI.autoConfirm });
    p = upsertPipeline(p.pipeline || p);
    if (UI.autoConfirm && p.status === 'awaiting_binding_confirm') p = upsertPipeline(await api('POST', `/api/pipelines/${enc(p.id)}/confirm`, {}));
    UI.drafts.delete(goalKey);
    UI.drafts.delete(bindKey);
    delete UI.parsePreviews[scope];
    UI.modal = null;
    APP.go('pipelines', p.id);
    return p;
  }, (p) => L(`${p.id} 已创建`, `${p.id} created`));
  return h('div', { class: 'modal-shell' },
    h('div', { class: 'sheet-handle', 'aria-hidden': 'true' }),
    h('div', { class: 'modal-head' }, h('h3', null, isDebug ? L('发起两速查错', 'New Two-Speed Debug') : L('发起新功能协作', 'New Feature Pipeline')), btn(L('关闭', 'Close'), { cls: 'ghost sm', onclick: closeModal })),
    h('div', { class: 'modal-body stack' },
      h('div', null, h('label', { class: 'lbl' }, isDebug ? L('故障现象 / 报错描述', 'Symptom / Error description') : L('需求目标', 'Feature Goal')),
        attachModalAutoGrow(field(goalKey, { tag: 'textarea', rows: 3, class: 'inp modal-autogrow-ta', placeholder: isDebug ? L('例如：POST /login 偶发 500', 'e.g. POST /login intermittently returns 500') : L('例如：给设置页增加导出 CSV 能力', 'e.g. Add CSV export to Settings page') }))),
      h('div', null, h('label', { class: 'lbl' }, L('模型绑定话术（选填，留空用默认配置）', 'Model routing instructions (optional, uses defaults if empty)')), bindInp),
      pv?.data ? h('div', { class: 'row' }, span('muted', L('解析预览：', 'Parsed bindings: ')), ...PLAYBOOK_STAGES[pb].map((k) => span('tag', `${stageName(k)}: ${shorthand(pv.data.resolved?.[k])}`))) : null,
      h('div', null, h('label', { class: 'lbl' }, L('项目目录（选填）', 'Project directory (optional)')), field(projKey, { class: 'inp mono', placeholder: L('留空自动使用当前会话目录', 'Defaults to current session cwd') }, curDir)),
      h('label', { class: 'row muted', style: 'font-size:13px;cursor:pointer' },
        h('input', { type: 'checkbox', checked: UI.autoConfirm, onchange: (e) => { UI.autoConfirm = e.target.checked; APP.schedule(); } }),
        L('自动确认绑定并立即启动首阶段', 'Auto-confirm bindings and start first stage immediately'))),
    h('div', { class: 'modal-foot' },
      btn(L('取消', 'Cancel'), { cls: 'ghost', onclick: closeModal }),
      btn(isDebug ? L('启动两速查错', 'Launch Debug') : L('启动新功能协作', 'Launch Pipeline'), { cls: 'primary', busyKey: `mcreate:${pb}`, onclick: submit })));
}

function authModalBody() {
  const save = () => {
    const t = draft('auth:token').trim();
    if (!t) { toast(L('请输入 token', 'Please enter a token'), 'warn'); return; }
    localStorage.setItem(TOKEN_KEY, t);
    document.cookie = `starfane_token=${enc(t)}; path=/; SameSite=Strict`;
    UI.drafts.delete('auth:token');
    S.authNeeded = false;
    S.healthFails = 0;
    connectSSE();
    loadAll();
  };
  const inp = field('auth:token', { class: 'inp mono', type: 'password', placeholder: L('粘贴 starfane_token…', 'Paste starfane_token…'), autocomplete: 'off' });
  inp.addEventListener('keydown', (e) => { if (e.key === 'Enter') save(); });
  return h('div', { class: 'modal-shell' },
    h('div', { class: 'sheet-handle', 'aria-hidden': 'true' }),
    h('div', { class: 'modal-head' }, h('h3', null, L('输入访问 Token', 'Authentication Required'))),
    h('div', { class: 'modal-body stack' },
      h('p', { class: 'muted' }, L('请粘贴本地 daemon 生成的 token（位于 ', 'Paste the token generated by your local daemon (at '), h('code', null, '<state>/web.token'), L('），或直接运行 ', '), or run '), h('code', null, 'starfane web'), L(' 自动登录。', ' to open pre-authenticated.')),
      inp),
    h('div', { class: 'modal-foot' }, btn(L('保存并连接', 'Save & Connect'), { cls: 'primary', onclick: save })));
}
