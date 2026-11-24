// starfane web console — Inbox, Pipelines, and Reviews views
import { AGENTS, DEPTHS, EFFORTS, SEVERITIES } from './constants.js';
import {
  agentTag,
  ago,
  bar,
  btn,
  deliveryText,
  dur,
  emptyState,
  errorBanner,
  field,
  h,
  kvGrid,
  modelTag,
  pct,
  plainList,
  selectField,
  shorthand,
  skeleton,
  span,
  statusInd,
  toast,
} from './dom.js';
import { act, api, artifact, ensureReview, loadPrecision, loadReviews, refreshReview } from './api.js';
import { renderInlineMd, renderMarkdown } from './markdown.js';
import {
  APP,
  S,
  UI,
  activeStage,
  allPipelines,
  base,
  draft,
  enc,
  inboxItems,
  isFinished,
  lastArtifact,
  paneDisplayId,
  short,
  stageName,
  stageOf,
  stateText,
  upsertPipeline,
} from './store.js';

export const focusBtn = (id, cls = 'ghost sm') => btn('切到终端', {
  cls,
  title: '切换终端焦点到该 pane',
  busyKey: `focus:${id}`,
  onclick: () => act(`focus:${id}`, () => api('POST', `/api/sessions/${enc(id)}/focus`, {}), (r) => {
    if (r?.switched_client) return `已切到 ${r.tmux_target || ''}`.trim();
    if (r?.attach_command) return `已选中 ${r.tmux_target || ''}（无已 attach 客户端，可运行 ${r.attach_command}）`;
    return '已切换终端焦点';
  }),
});

export function pipelineAction(p, payload, okMsg) {
  return act(`pa:${p.id}:${payload.action}`, async () => upsertPipeline(await api('POST', `/api/pipelines/${enc(p.id)}/action`, payload)), okMsg);
}

export function debugAction(p, verb, okMsg) {
  return act(`pa:${p.id}:${verb}`, async () => upsertPipeline(await api('POST', `/api/debug/${enc(p.id)}/${verb}`, {})), okMsg);
}

const cancelPipeBtn = (p) => btn('取消流程', {
  cls: 'ghost sm danger',
  busyKey: `pa:${p.id}:cancel`,
  onclick: () => { if (window.confirm(`确认取消流程 ${p.id}？`)) pipelineAction(p, { action: 'cancel' }, '流程已取消'); },
});

export function bindingEditor(p, scope = 'bind') {
  const rows = (p.stages || []).map((st) => {
    const b = st.binding || {};
    const k = `${scope}:${p.id}:${st.id}`;
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
    const b = st.binding || {};
    const k = `${scope}:${p.id}:${st.id}`;
    const agent = draft(`${k}:agent`, b.agent || '');
    const model = draft(`${k}:model`, b.model || '').trim();
    const effort = draft(`${k}:effort`, b.effort || '');
    const depth = draft(`${k}:depth`, b.depth || '');
    if (agent !== (b.agent || '') || model !== (b.model || '') || effort !== (b.effort || '') || depth !== (b.depth || '')) {
      out[st.id] = { ...b, agent: agent || null, model: model || null, effort: effort || null, depth: depth || null, same_as: agent ? null : (b.same_as ?? null) };
    }
  }
  return out;
}

export function confirmBindingBar(p, scope = 'bind') {
  return h('div', { class: 'row', style: 'justify-content:space-between;margin-top:12px' },
    span('muted', '可直接微调各阶段绑定，未修改项保持默认'),
    h('div', { class: 'row' },
      cancelPipeBtn(p),
      btn('确认并启动流程', {
        cls: 'primary sm',
        busyKey: `confirm:${p.id}`,
        onclick: () => {
          const overrides = collectOverrides(p, scope);
          act(`confirm:${p.id}`, async () => upsertPipeline(await api('POST', `/api/pipelines/${enc(p.id)}/confirm`, Object.keys(overrides).length ? { overrides } : {})), '流程已启动');
        },
      })));
}

export function planActionBar(p) {
  const nk = `chg:${p.id}`;
  return h('div', { class: 'stack-sm', style: 'margin-top:12px' },
    field(nk, { class: 'inp sm', placeholder: '如需退回修改，请在此填写修改说明…' }),
    h('div', { class: 'row', style: 'justify-content:flex-end' },
      cancelPipeBtn(p),
      btn('退回并要求修改', {
        cls: 'sm',
        busyKey: `pa:${p.id}:request_changes`,
        onclick: () => {
          const note = draft(nk).trim();
          if (!note) { toast('请先填写修改说明', 'warn'); return; }
          pipelineAction(p, { action: 'request_changes', note }, '已退回计划并附修改意见').then((r) => { if (r) UI.drafts.delete(nk); });
        },
      }),
      btn('批准计划并开始实现', { cls: 'primary sm', busyKey: `pa:${p.id}:approve_plan`, onclick: () => pipelineAction(p, { action: 'approve_plan' }, '计划已批准，进入实现阶段') })));
}

export function triageActionBar(p) {
  return h('div', { class: 'row', style: 'justify-content:flex-end;margin-top:12px' },
    cancelPipeBtn(p),
    btn('停止深挖', { cls: 'ghost sm', busyKey: `pa:${p.id}:stop`, onclick: () => debugAction(p, 'stop', '已停止深挖') }),
    btn('采用止血方案', { cls: 'sm', busyKey: `pa:${p.id}:mitigate`, onclick: () => debugAction(p, 'mitigate', '已采用止血建议') }),
    btn('继续深挖根因', { cls: 'primary sm', busyKey: `pa:${p.id}:continue`, onclick: () => debugAction(p, 'continue', '已进入 Phase B 深挖') }));
}

export function quickDispatchRow(key, target, placeholder = '直接向该会话发送回复…', btnCls = 'primary sm') {
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

export function viewInbox() {
  const items = inboxItems();
  return h('div', { class: 'stack' },
    h('div', { class: 'page-head' },
      h('div', null, h('h1', null, '待处理'), h('p', null, '需要人工确认或响应的事项 —— 权限审批、计划确认、快筛决策与评审裁决')),
      span('status-ind pill-soft', h('i', { class: `dot ${items.length ? 'dot-needs_permission' : 'dot-done'}` }), `${items.length} 项待处理`)),
    items.length
      ? items.map(inboxCard)
      : h('div', { class: 'card' }, emptyState('当前没有待处理事项', '当会话等待审批、计划待确认或排查得出初步假设时，会实时出现在这里。',
        btn('返回会话对话', { cls: 'sm', onclick: () => APP.go('workspace') }))));
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
          h('div', { class: 'row' }, btn('在抽屉查看完整设计', { cls: 'sm', onclick: () => APP.openDrawer('artifact', { artifactId: aid, title: `计划设计 · ${p.id}` }) })))
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
          h('div', { class: 'row' }, btn('查看完整快筛证据', { cls: 'sm', onclick: () => APP.openDrawer('artifact', { artifactId: aid, title: `快筛报告 · ${p.id}` }) })))
        : (aid ? skeleton(3) : h('div', { class: 'muted' }, '快筛工件加载中…')),
      triageActionBar(p),
    ];
  } else if (it.kind === 'needs_human_finding') {
    const rev = ensureReview(it.rid) || S.reviews.get(it.rid);
    const pend = rev ? (rev.findings || []).filter((f) => f.falsifier?.verdict === 'needs_human' && !f.feedback) : [];
    body = rev
      ? h('div', { class: 'stack-sm' },
        pend.length ? pend.map((af) => findingRow(rev, af)) : h('div', { class: 'muted' }, '该评审下待裁决条目已全部处理'),
        h('div', { class: 'row' }, btn('打开代码评审页', { cls: 'sm', onclick: () => APP.go('reviews', it.rid) })))
      : skeleton(3);
  } else if (s) {
    body = [
      s.last_error ? h('div', { class: 'callout err' }, span(null, s.last_error)) : null,
      s.last_message ? h('div', { class: 'callout' }, renderMarkdown(s.last_message)) : null,
      h('div', { class: 'row' },
        btn('在会话对话中打开', { cls: 'sm', onclick: () => APP.openSession(s.session_id) }),
        s.pane ? focusBtn(s.session_id) : null,
        (s.state === 'error' || s.last_error) ? btn('清除异常状态', { cls: 'sm danger', busyKey: `ack:${s.session_id}`, onclick: () => APP.ackSessionError(s) }) : null),
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
          s ? [agentTag(s.agent), s.pane ? span('tag host-pfx', paneDisplayId(s)) : null] : null,
          span('tabular', `已等待 ${dur(Date.now() - (it.since || Date.now()))}`)))),
    body);
}

export function viewPipelines() {
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
        btn('发起新功能协作', { cls: 'sm', onclick: () => { UI.modal = 'feature'; APP.schedule(); } }),
        btn('发起两速查错', { cls: 'sm', onclick: () => { UI.modal = 'debug'; APP.schedule(); } }))),
    h('div', { class: 'chips' }, filters.map(([k, l]) => h('button', {
      type: 'button',
      class: `chip${f === k ? ' on' : ''}`,
      onclick: () => { UI.pipeFilter = k; APP.schedule(); },
    }, l))),
    list.length
      ? list.map((p) => (p.playbook === 'debug' ? debugPipelineCard(p) : featurePipelineCard(p)))
      : h('div', { class: 'card' }, emptyState('当前筛选下没有流水线', '你可以在「会话对话」底部直接切换候选模式发起协作或查错，也可以点右上角按钮创建。')));
}

export function stageStepper(p, selKind, onSelect) {
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
      btn('流程元数据', { cls: 'ghost sm', onclick: () => APP.openDrawer('pipeline', { pipelineId: p.id }) })),
    stageStepper(p, sel, (k) => { UI.selStage[p.id] = k; APP.schedule(); }),
    st ? h('div', { class: 'stage-strip' },
      h('div', { class: 'row' },
        h('strong', null, stageName(st.id)),
        statusInd(st.status),
        st.binding?.agent ? agentTag(st.binding.agent) : null,
        modelTag(st.binding?.model),
        st.binding?.native ? span('tag soft-plum', `/${st.binding.native}`) : null,
        st.pane ? span('tag mono', st.pane) : null),
      h('div', { class: 'row' },
        st.session_id ? btn('打开会话', { cls: 'ghost sm', onclick: () => APP.openSession(st.session_id) }) : null,
        aid ? btn('查看阶段工件', { cls: 'sm', onclick: () => APP.openDrawer('artifact', { artifactId: aid, title: `${stageName(st.id)} · ${p.id}` }) }) : span('muted', '暂无工件'))) : null,
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
  const tri = stageOf(p, 'triage');
  const deep = stageOf(p, 'deep');
  const triId = lastArtifact(tri);
  const deepId = lastArtifact(deep);
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
          h('div', { class: 'row' }, btn('抽屉查看完整快筛', { cls: 'sm', onclick: () => APP.openDrawer('artifact', { artifactId: triId, title: `快筛报告 · ${p.id}` }) })))
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
          h('div', { class: 'row' }, btn('抽屉查看完整 RCA', { cls: 'sm', onclick: () => APP.openDrawer('artifact', { artifactId: deepId, title: `根因分析 · ${p.id}` }) })))
        : skeleton(3))
      : h('div', { class: 'muted' }, deep?.status === 'running' ? '正在复现并验证根本原因…' : '等待 Phase A 决策后启动'));
  return h('article', { class: `card${UI.arg === p.id ? ' focus-ring' : ''}`, id: `pl-${p.id}` },
    h('div', { class: 'card-head' },
      h('div', { class: 'grow' },
        h('div', { class: 'card-title' }, p.goal || '未命名排查'),
        h('div', { class: 'card-meta' }, h('code', null, p.id), statusInd(p.status), span('tag soft-amber', '两速查错'), span('muted', base(p.project?.root)), span('tabular', `更新于 ${ago(p.updated_ms)}`))),
      btn('流程元数据', { cls: 'ghost sm', onclick: () => APP.openDrawer('pipeline', { pipelineId: p.id }) })),
    p.status === 'awaiting_binding_confirm' ? [bindingEditor(p), confirmBindingBar(p)] : null,
    h('div', { class: 'phases' }, phaseA, phaseB));
}

export const g2Of = (af) => af.g2?.verdict || null;
export const g3Of = (af) => af.falsifier?.verdict || (af.disposition === 'pending_falsifier' ? 'pending_falsifier' : null);
export const effSev = (af) => (g3Of(af) === 'downgraded' && af.falsifier?.to ? af.falsifier.to : af.finding.severity);
export const isDropped = (af) => ['dropped', 'fabricated', 'returned_g1'].includes(af.disposition) || g3Of(af) === 'disproved' || g2Of(af) === 'fabricated';
const sevRank = (af) => SEVERITIES.indexOf(effSev(af));
export const sevTagCls = (s) => (s === 'blocker' ? 'soft-red' : s === 'major' ? 'soft-amber' : s === 'minor' ? 'soft-blue' : '');

export function dropReasons(af) {
  const r = [];
  if (g2Of(af) === 'fabricated' || af.disposition === 'fabricated') r.push('G2 证据不可验证或伪造');
  if (g3Of(af) === 'disproved') r.push('G3 被 Falsifier 证伪');
  if (af.disposition === 'returned_g1') r.push('G1 结构退回');
  return r.length ? r.join('；') : '已过滤';
}

export function viewReviews() {
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
      type: 'button',
      role: 'tab',
      class: `subtab${UI.revTab === k ? ' on' : ''}`,
      onclick: () => { UI.revTab = k; if (k === 'precision') loadPrecision(); APP.schedule(); },
    }, l))),
    body);
}

function reviewCard(rev) {
  const rp = rev.report || {};
  const fs = rev.findings || [];
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
        btn('在 Dropped Findings 中审计 →', { cls: 'ghost xs', onclick: () => { UI.revTab = 'dropped'; APP.schedule(); } })) : null));
}

export function findingRow(rev, af, { dropped = false } = {}) {
  const f = af.finding;
  const sev = effSev(af);
  const g2 = g2Of(af);
  const g3 = g3Of(af);
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
      btn('抽屉详情', { cls: 'xs', onclick: () => APP.openDrawer('finding', { reviewId: rev.id, findingId: f.id, dropped }) })));
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
  const v = sum('valid');
  const fp = sum('false_positive');
  const filterBar = h('div', { class: 'row', style: 'justify-content:space-between' },
    h('div', { class: 'chips' }, ['', ...AGENTS].map((a) => h('button', {
      type: 'button',
      class: `chip${UI.precAgent === a ? ' on' : ''}`,
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
