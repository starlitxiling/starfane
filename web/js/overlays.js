// starfane web console — Right Inspection Drawer & Modals (Spawn, Pipeline, Auth, Rename Host)
import { AGENTS, EFFORT_LABEL, NON_TMUX_GROUP, PLAYBOOK_STAGES, SEVERITIES, TOKEN_KEY } from './constants.js';
import {
  $,
  agentTag,
  ago,
  btn,
  clock,
  codeRefList,
  copyText,
  ctxBar,
  ctxPctOf,
  dur,
  emptyState,
  errorBanner,
  evBlocks,
  field,
  fmtBytes,
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
  shorthand,
  skeleton,
  span,
  statusInd,
  toast,
  tpsOf,
} from './dom.js';
import {
  act,
  api,
  artifact,
  connectSSE,
  loadAll,
  loadHandoffs,
  loadModels,
  loadPreview,
  loadSessionOutput,
  loadTmuxSessions,
  refreshReview,
  renameHostApi,
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
  hostDisplayName,
  hostOf,
  hydrateSpawnAgent,
  isTmuxSession,
  knownProjects,
  loadSpawnPrefs,
  modelBaseOf,
  modelsForAgent,
  muxKindOf,
  paneDisplayId,
  pvKeyOf,
  saveSpawnPrefs,
  sessionLabel,
  short,
  stageName,
  stateText,
  subagentDisplayName,
  subagentsOf,
  tmuxGroupOf,
  upsertPipeline,
} from './store.js';
import { formatTerminalText, renderChunkedTerminal } from './terminal.js';
import { dropReasons, effSev, focusBtn, g2Of, g3Of, sevTagCls } from './views.js';

export function openDrawer(kind, params) {
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
  loadModels();
  loadTmuxSessions();
  const prefs = loadSpawnPrefs();
  const ag = prefs.last_agent || 'claude';
  UI.drafts.set('sp:agent', ag);
  UI.drafts.delete('sp:dir');
  hydrateSpawnAgent(ag, prefs);
  UI.modal = 'spawn';
  APP.schedule();
}

export function openRenameHostModal(hostId) {
  UI.renameHostId = hostId;
  UI.drafts.set('host:rename', hostDisplayName(hostId));
  UI.modal = 'rename-host';
  APP.schedule();
}

export function closeModal() {
  UI.modal = null;
  UI.renameHostId = null;
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
  let title = '详情检查器';
  let sub = '';
  let body = null;
  if (d.kind === 'session') {
    const s = S.sessions.get(d.sessionId);
    title = s ? `会话详情 · ${sessionLabel(s)}` : '会话详情';
    sub = s ? s.session_id : '';
    body = s ? drawerSession(s) : emptyState('会话不存在', '该会话可能已被清理。');
  } else if (d.kind === 'terminal') {
    const dMode = UI.drawerPvLines;
    title = `完整终端输出 · ${d.title || d.pane}`;
    sub = `${d.pane}（${dMode === 'all' ? '全部滚动历史' : `最近 ${dMode} 行`}）`;
    const pv = S.previews.get(pvKeyOf(d.pane, dMode));
    const formatted = pv?.text ? formatTerminalText(pv.text) : '';
    body = !pv || pv.state === 'loading' ? skeleton(8)
      : pv.state === 'err' ? errorBanner(`读取终端失败：${pv.err}`, '重试', () => loadPreview(d.pane, { mode: dMode }))
      : h('div', { class: 'stack-sm' },
        h('div', { class: 'row', style: 'justify-content:space-between' },
          span('muted tabular', `共 ${formatted ? formatted.split('\n').length : 0} 行 · 更新于 ${clock(pv.updatedMs)}`),
          h('div', { class: 'row-nowrap', style: 'gap:6px' },
            h('div', { class: 'view-seg' }, [1000, 'all'].map((m) => h('button', {
              type: 'button',
              class: dMode === m ? 'on' : '',
              onclick: () => { UI.drawerPvLines = m; loadPreview(d.pane, { mode: m }); APP.schedule(); },
            }, m === 'all' ? '全部历史' : `${m} 行`))),
            btn('刷新', { cls: 'sm', onclick: () => loadPreview(d.pane, { mode: dMode }) }))),
        formatted ? h('div', { class: 'callout', style: 'background:var(--terminal-bg)' }, renderChunkedTerminal(formatted)) : h('pre', { class: 'code-block' }, '（无输出）'));
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
  const hid = hostOf(s);
  return h('div', { class: 'stack' },
    h('div', { class: 'drawer-controls' },
      s.pane ? focusBtn(s.session_id, 'sm') : null,
      (s.state === 'error' || s.state === 'needs_permission' || s.state === 'needs_input' || s.last_error)
        ? btn('清除异常状态', { cls: 'sm danger', busyKey: `ack:${s.session_id}`, onclick: () => APP.ackSessionError(s) }) : null,
      btn('手动刷新', { cls: 'sm', onclick: () => { loadSessionOutput(s, { silent: false, forcePreview: true }); toast('已刷新会话输出'); } }),
      s.pane ? btn('完整终端日志', { cls: 'ghost sm', onclick: () => openDrawer('terminal', { pane: s.pane, title: sessionLabel(s) }) }) : null,
      h('label', { class: 'row muted', style: 'cursor:pointer;font-size:12.5px;margin-left:auto' },
        h('input', { type: 'checkbox', checked: UI.autoRefreshPreview, onchange: (e) => { UI.autoRefreshPreview = e.target.checked; APP.rerender(); } }), '3s 自动刷新')),
    drawerMetricsCard(s.metrics),
    kvGrid([
      ['所属主机', h('span', { class: 'row' }, h('strong', null, hostDisplayName(hid, s)), h('code', null, hid))],
      ['复用器', span(muxKindOf(s) === 'zellij' ? 'tag mux-zj' : 'tag', muxKindOf(s))],
      ['会话 ID', h('code', null, s.session_id)],
      ['窗口标识', s.tmux_label ? h('code', null, s.tmux_label) : null],
      ['Agent / 模型', h('div', { class: 'row' }, agentTag(s.agent), modelTag(modelBaseOf(s.model, s.effort) || s.model))],
      ['思考强度', s.effort ? span('tag mono', s.effort) : span('muted', '—')],
      ['状态', statusInd(s.state, `${stateText(s.state)}（持续 ${dur(Date.now() - (s.state_since_ms || Date.now()))}）`)],
      s.last_error ? ['最近错误', span('mono', s.last_error)] : null,
      ['来源', s.discovered_only ? span('tag legacy', '终端扫描发现') : span('tag soft-green', 'Hook 实时直连')],
      ['终端位置', s.tmux_target ? h('code', null, `${s.tmux_target} (${paneDisplayId(s) || '—'})`) : h('code', null, paneDisplayId(s) || '—')],
      ['工作目录', h('code', null, s.cwd || '—')],
      ['Git 根目录', h('code', null, s.git_root || '—')],
      ['最近活动', ago(s.last_event_ms)],
    ].filter(Boolean)),
    h('div', { class: 'stack-xs' },
      h('div', { class: 'sec-h' }, `子会话 / Swarm（${subs.length || s.subagent_count || 0}）`),
      subs.length ? subs.map((c) => h('button', {
        type: 'button',
        class: 'callout row sub-drawer-btn',
        style: 'justify-content:space-between',
        onclick: () => { closeDrawer(); APP.openSession(c.session_id); },
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
  if (a.state === 'err') return errorBanner(`工件加载失败：${a.err}`, '重试', () => { S.artifacts.delete(id); APP.schedule(); });
  const d = a.data || {};
  const hd = d.header || {};
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

export function renderModal() {
  const d = $('#modal');
  const want = S.authNeeded ? 'auth' : UI.modal;
  if (!want) { if (d.open) d.close(); return; }
  if (!d.open) d.showModal();
  if (want === 'auth') d.replaceChildren(authModalBody());
  else if (want === 'spawn') d.replaceChildren(spawnModalBody());
  else if (want === 'rename-host') d.replaceChildren(renameHostModalBody());
  else d.replaceChildren(pipelineModalBody(want));
}

function renameHostModalBody() {
  const hostId = UI.renameHostId || 'local';
  const curName = hostDisplayName(hostId);
  const submit = () => {
    const next = draft('host:rename', curName).trim();
    if (!next) { toast('主机名称不能为空', 'warn'); return; }
    renameHostApi(hostId, next);
  };
  const inp = field('host:rename', {
    class: 'inp',
    placeholder: '输入主机显示名称（如 腾讯云 GPU / 办公 Mac）…',
    autocomplete: 'off',
  }, curName);
  inp.addEventListener('keydown', (e) => { if (e.key === 'Enter' && !e.isComposing) { e.preventDefault(); submit(); } });
  return h('div', { class: 'stack' },
    h('div', { class: 'modal-head' }, h('h3', null, '重命名主机'), btn('关闭', { cls: 'ghost sm', onclick: closeModal })),
    h('p', { class: 'muted' }, '仅修改显示名称，不可变主机 ID ', h('code', null, hostId), ' 与面板前缀 ', h('code', null, `${hostId}:%...`), ' 保持稳定。'),
    h('div', null, h('label', { class: 'lbl' }, '主机显示名称'), inp),
    h('div', { class: 'row', style: 'justify-content:flex-end' },
      btn('取消', { cls: 'ghost', onclick: closeModal }),
      btn('保存名称', { cls: 'primary', busyKey: `rename-host:${hostId}`, onclick: submit })));
}

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
  const sessDlId = 'sp-sessions';
  const activeSessionGroups = (S.tmuxSessions || []).length
    ? S.tmuxSessions
    : [...S.sessions.values()]
      .filter((s) => s.state !== 'ended' && isTmuxSession(s))
      .map((s) => tmuxGroupOf(s))
      .filter((g) => g && g !== NON_TMUX_GROUP && g !== 'tmux');
  const sessionCandidates = [...new Set(['agent-session', ...activeSessionGroups])];
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
    const tmuxSess = draft('sp:session').trim();
    const payload = {
      agent: ag,
      project_root: proj || undefined,
      tmux_session: tmuxSess || undefined,
      model: model || undefined,
      effort: pickedEff || undefined,
      name: draft('sp:name').trim() || undefined,
      prompt: draft('sp:prompt').trim() || undefined,
    };
    const r = await api('POST', '/api/sessions/spawn', payload);
    saveSpawnPrefs(ag, { model: model || undefined, effort: pickedEff || '' });
    ['sp:dir', 'sp:session', 'sp:name', 'sp:prompt'].forEach((k) => UI.drafts.delete(k));
    UI.modal = null;
    await loadAll();
    if (r?.session_id) APP.openSession(r.session_id);
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
    APP.rerender();
  });
  const effortSelect = selectField(effKey, effortOpts, fixedEffort ? '' : curEff, {
    class: 'inp', style: 'width:100%', disabled: fixedEffort,
  });
  effortSelect.addEventListener('change', () => { persistCurrent(agent, curSel, effortSelect.value); });
  const customModelInp = curSel === '__custom__' ? field('sp:modelCustom', { class: 'inp sm mono', placeholder: '输入自定义模型 ID…', autocomplete: 'off' }) : null;
  if (customModelInp) customModelInp.addEventListener('input', () => persistCurrent(agent, '__custom__', fixedEffort ? '' : curEff));
  return h('div', { class: 'stack' },
    h('div', { class: 'modal-head' }, h('h3', null, '新建交互会话'), btn('关闭', { cls: 'ghost sm', onclick: closeModal })),
    h('datalist', { id: dlId }, projects.map((p) => h('option', { value: p }))),
    h('datalist', { id: sessDlId }, sessionCandidates.map((s) => h('option', { value: s }))),
    h('div', null,
      h('label', { class: 'lbl' }, '选择 Agent'),
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
      h('div', { class: 'stack-xs' }, h('label', { class: 'lbl', style: 'margin:0' }, '模型'), modelSelect, customModelInp),
      h('div', { class: 'stack-xs' }, h('label', { class: 'lbl', style: 'margin:0' }, '思考强度'), effortSelect)),
    h('div', null,
      h('label', { class: 'lbl' }, '工作目录（默认 ~/.agent_session）'),
      field('sp:dir', { class: 'inp mono', list: dlId, placeholder: '留空默认 ~/.agent_session', autocomplete: 'off' }, curDir)),
    h('div', { class: 'opt-grid' },
      h('div', null, h('label', { class: 'lbl' }, 'tmux 会话（Session，选填）'),
        field('sp:session', { class: 'inp mono', list: sessDlId, placeholder: '默认 agent-session（自动创建）', autocomplete: 'off' })),
      h('div', null, h('label', { class: 'lbl' }, 'tmux 窗口名（Window，选填）'),
        field('sp:name', { class: 'inp mono', placeholder: `留空顺延命名（如 ${agent}-1）`, autocomplete: 'off' }))),
    h('div', null, h('label', { class: 'lbl' }, '首条消息（选填）'),
      field('sp:prompt', { tag: 'textarea', rows: 2, class: 'inp', placeholder: '留空则仅打开干净的交互式会话…' })),
    h('div', { class: 'row', style: 'justify-content:flex-end' },
      btn('取消', { cls: 'ghost', onclick: closeModal }),
      btn('新建交互会话', { cls: 'primary', busyKey: 'spawn-sess', onclick: create })));
}

function pipelineModalBody(pb) {
  const isDebug = pb === 'debug';
  const scope = `modal:${pb}`;
  const goalKey = `${scope}:goal`;
  const bindKey = `${scope}:bind`;
  const projKey = `${scope}:proj`;
  const curDir = currentSession()?.git_root || currentSession()?.cwd || knownProjects()[0] || '';
  const pv = UI.parsePreviews[scope];
  const bindInp = field(bindKey, { class: 'inp', placeholder: isDebug ? '选填：如 快速排查用 agy，深挖根因用 codex' : '选填：如 计划用 claude，实现用 codex，Review 用 agy' });
  bindInp.addEventListener('input', () => queueParse(scope, pb, projKey, bindKey, curDir));
  const submit = () => act(`mcreate:${pb}`, async () => {
    const goal = draft(goalKey).trim();
    const utt = draft(bindKey).trim();
    const proj = draft(projKey, curDir).trim();
    const combined = [goal, utt].filter(Boolean).join('；');
    if (!combined) throw new Error(isDebug ? '请描述故障现象' : '请描述需求目标');
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
  }, (p) => `${p.id} 已创建`);
  return h('div', { class: 'stack' },
    h('div', { class: 'modal-head' }, h('h3', null, isDebug ? '发起两速查错' : '发起新功能协作'), btn('关闭', { cls: 'ghost sm', onclick: closeModal })),
    h('div', null, h('label', { class: 'lbl' }, isDebug ? '故障现象 / 报错描述' : '需求目标'),
      field(goalKey, { tag: 'textarea', rows: 3, class: 'inp', placeholder: isDebug ? '例如：POST /login 偶发 500' : '例如：给设置页增加导出 CSV 能力' })),
    h('div', null, h('label', { class: 'lbl' }, '模型绑定话术（选填，留空用默认配置）'), bindInp),
    pv?.data ? h('div', { class: 'row' }, span('muted', '解析预览：'), ...PLAYBOOK_STAGES[pb].map((k) => span('tag', `${stageName(k)}: ${shorthand(pv.data.resolved?.[k])}`))) : null,
    h('div', null, h('label', { class: 'lbl' }, '项目目录（选填）'), field(projKey, { class: 'inp mono', placeholder: '留空自动使用当前会话目录' }, curDir)),
    h('label', { class: 'row muted', style: 'font-size:13px;cursor:pointer' },
      h('input', { type: 'checkbox', checked: UI.autoConfirm, onchange: (e) => { UI.autoConfirm = e.target.checked; APP.schedule(); } }),
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
    document.cookie = `starfane_token=${enc(t)}; path=/; SameSite=Strict`;
    UI.drafts.delete('auth:token');
    S.authNeeded = false;
    S.healthFails = 0;
    connectSSE();
    loadAll();
  };
  const inp = field('auth:token', { class: 'inp mono', type: 'password', placeholder: '粘贴 starfane_token…', autocomplete: 'off' });
  inp.addEventListener('keydown', (e) => { if (e.key === 'Enter') save(); });
  return h('div', { class: 'stack' },
    h('div', { class: 'modal-head' }, h('h3', null, '输入访问 Token')),
    h('p', { class: 'muted' }, '请粘贴本地 daemon 生成的 token（位于 ', h('code', null, '<state>/web.token'), '），或直接运行 ', h('code', null, 'starfane web'), ' 自动登录。'),
    inp,
    h('div', { class: 'row', style: 'justify-content:flex-end' }, btn('保存并连接', { cls: 'primary', onclick: save })));
}
