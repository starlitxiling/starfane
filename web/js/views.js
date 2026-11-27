// starfane web console — Inbox, Pipelines, and Reviews views
import { AGENTS, DEPTHS, EFFORTS, SEVERITIES } from './constants.js';
import { L } from './i18n.js';
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
import {
  act,
  api,
  artifact,
  ensureReview,
  loadPrecision,
  loadPreview,
  loadReviews,
  refreshReview,
  sendSessionKeyApi,
} from './api.js';
import { renderInlineMd, renderMarkdown } from './markdown.js';
import { extractInteractiveMenuSnippet } from './terminal.js';
import {
  APP,
  S,
  UI,
  activeStage,
  allPipelines,
  base,
  draft,
  enc,
  hashStr,
  hostOf,
  inboxItems,
  isFinished,
  lastArtifact,
  paneDisplayId,
  pvKeyOf,
  pvModeOf,
  sessionLabel,
  short,
  stageName,
  stageOf,
  stateText,
  upsertPipeline,
} from './store.js';

export const focusBtn = (id, cls = 'ghost sm') => btn(L('切到终端', 'Focus pane'), {
  cls,
  title: L('切换终端焦点到该 pane', 'Switch terminal focus to this pane'),
  busyKey: `focus:${id}`,
  onclick: () => act(`focus:${id}`, () => api('POST', `/api/sessions/${enc(id)}/focus`, {}), (r) => {
    if (r?.switched_client) return L(`已切到 ${r.tmux_target || ''}`, `Switched to ${r.tmux_target || ''}`).trim();
    if (r?.attach_command) return L(`已选中 ${r.tmux_target || ''}（无已 attach 客户端，可运行 ${r.attach_command}）`, `Selected ${r.tmux_target || ''} (no attached client; run ${r.attach_command})`);
    return L('已切换终端焦点', 'Terminal focus switched');
  }),
});

export function isInteractivePromptSession(s) {
  return Boolean(s && !s.is_subagent && (s.state === 'needs_permission' || s.state === 'needs_input'));
}

export function promptPreviewSourceFor(s) {
  if (!s) return { text: '', hash: '' };
  const tr = S.transcripts.get(s.session_id);
  const trTail = typeof tr?.live_tail === 'string' && tr.live_tail.trim()
    ? { text: tr.live_tail, hash: `tr:${hashStr(tr.live_tail)}`, updatedMs: tr.liveTailUpdatedMs || tr.updatedMs || 0 }
    : null;
  const mapTailRaw = S.liveTails.get(s.session_id);
  const mapTail = mapTailRaw?.text?.trim()
    ? { text: mapTailRaw.text, hash: `lt:${mapTailRaw.hash}`, updatedMs: mapTailRaw.updatedMs || 0 }
    : null;
  const sseTail = trTail && mapTail
    ? (trTail.updatedMs >= mapTail.updatedMs ? trTail : mapTail)
    : (trTail || mapTail);

  let pv = null;
  if (s.pane) {
    const direct = S.previews.get(pvKeyOf(s.pane));
    if (direct?.text?.trim()) {
      pv = direct;
    } else {
      const pfx = `${s.pane}:`;
      for (const [k, entry] of S.previews.entries()) {
        if (k.startsWith(pfx) && entry?.text?.trim()) {
          if (!pv || (entry.updatedMs || 0) >= (pv.updatedMs || 0)) pv = entry;
        }
      }
    }
  }
  if (sseTail && (!pv || sseTail.updatedMs >= (pv.updatedMs || 0))) {
    return { text: sseTail.text, hash: sseTail.hash };
  }
  if (pv?.text) {
    return { text: pv.text, hash: `pv:${pv.hash || hashStr(pv.text)}` };
  }
  return { text: '', hash: '' };
}

export function renderInteractivePromptBar(s, opts = {}) {
  if (!isInteractivePromptSession(s)) return null;
  const variant = opts.variant === 'inbox' ? 'inbox' : 'workspace';
  const sid = s.session_id;
  const pvSrc = promptPreviewSourceFor(s);
  if (opts.autoFetchPreview && s.pane && !pvSrc.text) {
    const hid = hostOf(s);
    const offline = hid !== 'local' && S.hosts.get(hid)?.status === 'offline';
    if (!offline && !S.previews.has(pvKeyOf(s.pane))) {
      loadPreview(s.pane, { mode: pvModeOf(s.pane), silent: true });
    }
  }
  const snippet = pvSrc.text ? extractInteractiveMenuSnippet(pvSrc.text, variant === 'inbox' ? 6 : 5) : '';
  const sessionBusy = UI.busy.has(`key:${sid}`);
  const canSend = Boolean(s.pane && s.state !== 'ended');
  const reason = String(s.last_error || '').trim();

  const keySpecs = [
    {
      key: 'Up',
      label: L('↑ 上移', '↑ Up'),
      cls: 'prompt-action-btn',
      title: L('向终端发送方向上键（Up）', 'Send Up arrow key to terminal'),
    },
    {
      key: 'Down',
      label: L('↓ 下移', '↓ Down'),
      cls: 'prompt-action-btn',
      title: L('向终端发送方向下键（Down）', 'Send Down arrow key to terminal'),
    },
    {
      key: 'Enter',
      label: L('↵ 确认 (Enter)', '↵ Confirm (Enter)'),
      cls: 'primary prompt-action-btn is-confirm',
      title: L('向终端发送回车键确认当前选项（Enter）', 'Send Enter key to confirm selected option'),
    },
    {
      key: 'Escape',
      label: L('Esc 取消', 'Esc Cancel'),
      cls: 'prompt-action-btn is-cancel',
      title: L('向终端发送 Esc 键取消或返回（Escape）', 'Send Escape key to cancel or go back'),
    },
  ];

  const makeKeyBtn = (spec) => {
    const isSelfBusy = UI.busy.has(`key:${sid}:${spec.key}`);
    return h('button', {
      type: 'button',
      class: `btn ${spec.cls}${isSelfBusy ? ' is-busy' : ''}`,
      'data-prompt-key': spec.key,
      'data-prompt-sid': sid,
      disabled: !canSend || sessionBusy,
      title: canSend ? spec.title : L('该会话未绑定终端 pane，无法发送交互按键', 'No terminal pane attached — cannot send interactive keys'),
      'aria-label': spec.title,
      onclick: () => sendSessionKeyApi(sid, spec.key),
    }, spec.label);
  };

  const previewEl = snippet
    ? h('pre', { class: 'prompt-action-preview', 'aria-label': L('终端交互菜单预览', 'Interactive terminal menu preview') }, snippet)
    : null;
  if (previewEl && typeof requestAnimationFrame === 'function') {
    requestAnimationFrame(() => {
      if (previewEl.isConnected && previewEl.scrollHeight > previewEl.clientHeight + 2) {
        previewEl.scrollTop = previewEl.scrollHeight;
      }
    });
  }

  return h('div', {
    class: `prompt-action-card is-${variant}`,
    role: 'region',
    'aria-label': L('终端交互确认操作栏', 'Interactive terminal confirmation controls'),
    'data-prompt-bar-sid': sid,
  },
  h('div', { class: 'prompt-action-head' },
    h('div', { class: 'prompt-action-title-wrap' },
      h('i', { class: `dot dot-${s.state}`, 'aria-hidden': 'true' }),
      h('strong', { class: 'prompt-action-title' }, L('等待终端交互确认', 'Interactive confirmation required'))),
    reason ? span('tag prompt-action-reason', reason) : null),
  previewEl
    || h('div', { class: 'prompt-action-hint' }, L('使用下方按键直接在远端/本机终端中选择并确认', 'Use the buttons below to navigate and confirm directly in the terminal')),
  !canSend
    ? h('div', { class: 'prompt-action-nopane' }, L('当前会话未绑定终端 pane，无法发送交互按键', 'No terminal pane attached — interactive keys unavailable'))
    : null,
  h('div', { class: 'prompt-action-btns' }, keySpecs.map(makeKeyBtn)));
}

export function pipelineAction(p, payload, okMsg) {
  return act(`pa:${p.id}:${payload.action}`, async () => upsertPipeline(await api('POST', `/api/pipelines/${enc(p.id)}/action`, payload)), okMsg);
}

export function debugAction(p, verb, okMsg) {
  return act(`pa:${p.id}:${verb}`, async () => upsertPipeline(await api('POST', `/api/debug/${enc(p.id)}/${verb}`, {})), okMsg);
}

const cancelPipeBtn = (p) => btn(L('取消流程', 'Cancel pipeline'), {
  cls: 'ghost sm danger',
  busyKey: `pa:${p.id}:cancel`,
  onclick: () => { if (window.confirm(L(`确认取消流程 ${p.id}？`, `Cancel pipeline ${p.id}?`))) pipelineAction(p, { action: 'cancel' }, L('流程已取消', 'Pipeline cancelled')); },
});

export function bindingEditor(p, scope = 'bind') {
  const rows = (p.stages || []).map((st) => {
    const b = st.binding || {};
    const k = `${scope}:${p.id}:${st.id}`;
    const agentOpts = [...(b.agent ? [] : [['', b.same_as ? L(`同 ${stageName(b.same_as)}`, `same as ${stageName(b.same_as)}`) : L('默认', 'default')]]), ...AGENTS.map((a) => [a, a])];
    return h('tr', null,
      h('td', { style: 'font-weight:600;white-space:nowrap' }, stageName(st.id)),
      h('td', null, selectField(`${k}:agent`, agentOpts, b.agent || '', { class: 'inp sm' })),
      h('td', null, field(`${k}:model`, { class: 'inp sm mono', placeholder: L('默认模型', 'Default model'), autocomplete: 'off' }, b.model || '')),
      h('td', null, selectField(`${k}:effort`, [['', '—'], ...EFFORTS.map((e) => [e, e])], b.effort || '', { class: 'inp sm' })),
      h('td', null, h('div', { class: 'row' },
        st.id === 'test_review' ? selectField(`${k}:depth`, [['', 'D1'], ...DEPTHS.map((d) => [d, d])], b.depth || '', { class: 'inp sm' }) : null,
        b.native ? span('tag soft-plum', `/${b.native}`) : null)));
  });
  return h('div', { class: 'table-wrap' }, h('table', { class: 'tbl' },
    h('thead', null, h('tr', null, [L('阶段', 'Stage'), 'Agent', 'Model', 'Effort', L('Depth / 原生模式', 'Depth / Native mode')].map((x) => h('th', null, x)))),
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
    span('muted', L('可直接微调各阶段绑定，未修改项保持默认', 'Adjust stage bindings below; untouched stages stay default')),
    h('div', { class: 'row' },
      cancelPipeBtn(p),
      btn(L('确认并启动流程', 'Confirm & start'), {
        cls: 'primary sm',
        busyKey: `confirm:${p.id}`,
        onclick: () => {
          const overrides = collectOverrides(p, scope);
          act(`confirm:${p.id}`, async () => upsertPipeline(await api('POST', `/api/pipelines/${enc(p.id)}/confirm`, Object.keys(overrides).length ? { overrides } : {})), L('流程已启动', 'Pipeline started'));
        },
      })));
}

export function planActionBar(p) {
  const nk = `chg:${p.id}`;
  return h('div', { class: 'stack-sm', style: 'margin-top:12px' },
    field(nk, { class: 'inp sm', placeholder: L('如需退回修改，请在此填写修改说明…', 'Enter change request notes if rejecting…') }),
    h('div', { class: 'row', style: 'justify-content:flex-end' },
      cancelPipeBtn(p),
      btn(L('退回并要求修改', 'Request changes'), {
        cls: 'sm',
        busyKey: `pa:${p.id}:request_changes`,
        onclick: () => {
          const note = draft(nk).trim();
          if (!note) { toast(L('请先填写修改说明', 'Please enter change notes first'), 'warn'); return; }
          pipelineAction(p, { action: 'request_changes', note }, L('已退回计划并附修改意见', 'Plan returned with change notes')).then((r) => { if (r) UI.drafts.delete(nk); });
        },
      }),
      btn(L('批准计划并开始实现', 'Approve plan & implement'), { cls: 'primary sm', busyKey: `pa:${p.id}:approve_plan`, onclick: () => pipelineAction(p, { action: 'approve_plan' }, L('计划已批准，进入实现阶段', 'Plan approved; moving to implementation')) })));
}

export function triageActionBar(p) {
  return h('div', { class: 'row', style: 'justify-content:flex-end;margin-top:12px' },
    cancelPipeBtn(p),
    btn(L('停止深挖', 'Stop debug'), { cls: 'ghost sm', busyKey: `pa:${p.id}:stop`, onclick: () => debugAction(p, 'stop', L('已停止深挖', 'Debug stopped')) }),
    btn(L('采用止血方案', 'Apply mitigation'), { cls: 'sm', busyKey: `pa:${p.id}:mitigate`, onclick: () => debugAction(p, 'mitigate', L('已采用止血建议', 'Mitigation selected')) }),
    btn(L('继续深挖根因', 'Continue Deep RCA'), { cls: 'primary sm', busyKey: `pa:${p.id}:continue`, onclick: () => debugAction(p, 'continue', L('已进入 Phase B 深挖', 'Moved to Phase B Deep RCA')) }));
}

export function quickDispatchRow(key, target, placeholder = L('直接向该会话发送回复…', 'Send a reply directly to this session…'), btnCls = 'primary sm') {
  const k = `qmsg:${key}`;
  const inp = field(k, { class: 'inp sm', placeholder, autocomplete: 'off' });
  const send = () => {
    const text = (draft(k) || inp.value || '').trim();
    if (!text) return;
    act(k, async () => {
      const r = await api('POST', '/api/dispatch', { target, text, interactive: true });
      UI.drafts.delete(k);
      return r;
    }, (r) => ({ msg: L(`已发送：${deliveryText(r)}`, `Sent: ${deliveryText(r)}`), kind: r && (r.status === 'failed' || r.status === 'unconfirmed') ? 'warn' : 'ok' }));
  };
  inp.addEventListener('keydown', (e) => { if (e.key === 'Enter' && !e.isComposing) { e.preventDefault(); send(); } });
  return h('div', { class: 'row-nowrap', style: 'margin-top:10px' }, inp, btn(L('发送到会话', 'Send'), { cls: btnCls, busyKey: k, onclick: send }));
}

function kindInfo(kind) {
  return {
    session_alert: [L('会话等待响应', 'Session Needs Input'), 'needs_permission'],
    binding_confirm: [L('绑定待确认', 'Confirm Bindings'), 'awaiting_binding_confirm'],
    plan_approval: [L('计划待批准', 'Plan Approval'), 'awaiting_human'],
    triage_ready: [L('快筛结论待决', 'Triage Decision'), 'awaiting_human'],
    blocked: [L('流程阻塞', 'Pipeline Blocked'), 'blocked'],
    needs_human_finding: [L('评审待裁决', 'Finding Review'), 'needs_human'],
    delivery_unconfirmed: [L('投递未确认', 'Delivery Unconfirmed'), 'needs_permission'],
  }[kind] || [kind, 'awaiting_human'];
}

export function viewInbox() {
  const items = inboxItems();
  return h('div', { class: 'stack' },
    h('div', { class: 'page-head' },
      h('div', null, h('h1', null, L('待处理', 'Inbox')), h('p', null, L('需要人工确认或响应的事项 —— 权限审批、计划确认、快筛决策与评审裁决', 'Items awaiting human approval or input — permissions, plans, triage decisions, and findings'))),
      span('status-ind pill-soft', h('i', { class: `dot ${items.length ? 'dot-needs_permission' : 'dot-done'}` }), L(`${items.length} 项待处理`, `${items.length} pending`))),
    items.length
      ? items.map(inboxCard)
      : h('div', { class: 'card' }, emptyState(
        L('当前没有待处理事项', 'Nothing in Inbox'),
        L('当会话等待审批、计划待确认或排查得出初步假设时，会实时出现在这里。', 'Sessions needing input, plans awaiting approval, or triage hypotheses will appear here in real time.'),
        btn(L('返回会话对话', 'Back to Workspace'), { cls: 'sm', onclick: () => APP.go('workspace') }))));
}

function inboxCardTitle(it, p, s, kLabel) {
  if (it.kind === 'binding_confirm' && p) {
    return L(`确认绑定：${p.goal || p.id}`, `Confirm bindings: ${p.goal || p.id}`);
  }
  if (it.kind === 'plan_approval' && p) {
    return L(`批准计划：${p.goal || p.id}`, `Approve plan: ${p.goal || p.id}`);
  }
  if (it.kind === 'triage_ready' && p) {
    return L(`快筛结论待决：${p.goal || p.id}`, `Triage decision needed: ${p.goal || p.id}`);
  }
  if (it.kind === 'needs_human_finding' && it.rid) {
    return L(`评审 ${short(it.rid, 8)} 有待人工判断的发现`, `Review ${short(it.rid, 8)} has findings awaiting review`);
  }
  if (it.kind === 'delivery_unconfirmed' && s) {
    return L(`${sessionLabel(s)} · 投递未确认`, `${sessionLabel(s)} · Delivery unconfirmed`);
  }
  if (s) {
    return `${sessionLabel(s)} · ${stateText(s.state)}`;
  }
  if (p) {
    const ah = (p.stages || []).find((st) => st.status === 'awaiting_human');
    if (ah) {
      return L(`${stageName(ah.id)} 需要人工处理：${p.goal || p.id}`, `${stageName(ah.id)} needs human action: ${p.goal || p.id}`);
    }
    return L(`流程阻塞：${p.goal || p.id}`, `Pipeline blocked: ${p.goal || p.id}`);
  }
  return it.title || kLabel;
}

function inboxCard(it) {
  const [kLabel, dotKind] = kindInfo(it.kind);
  const p = it.pid ? S.pipelines.get(it.pid) : null;
  const s = it.sid ? S.sessions.get(it.sid) : null;
  const cardTitle = inboxCardTitle(it, p, s, kLabel);
  let body = null;
  if (it.kind === 'binding_confirm' && p) {
    body = [bindingEditor(p), confirmBindingBar(p)];
  } else if (it.kind === 'plan_approval' && p) {
    const aid = lastArtifact(stageOf(p, 'plan'));
    const a = aid ? artifact(aid) : null;
    body = [
      a?.state === 'ok' && a.data
        ? h('div', { class: 'stack-sm' },
          kvGrid([[L('目标', 'Goal'), a.data.goal], [L('任务数', 'Tasks'), L(`${(a.data.tasks || []).length} 项`, `${(a.data.tasks || []).length}`)], [L('验收标准', 'Acceptance'), plainList(a.data.acceptance)]]),
          h('div', { class: 'row' }, btn(L('在抽屉查看完整设计', 'Open full plan in drawer'), { cls: 'sm', onclick: () => APP.openDrawer('artifact', { artifactId: aid, title: L(`计划设计 · ${p.id}`, `Plan · ${p.id}`) }) })))
        : (aid ? skeleton(3) : h('div', { class: 'muted' }, L('计划工件尚未生成', 'Plan artifact not yet generated'))),
      planActionBar(p),
    ];
  } else if (it.kind === 'triage_ready' && p) {
    const aid = lastArtifact(stageOf(p, 'triage'));
    const a = aid ? artifact(aid) : null;
    body = [
      a?.state === 'ok' && a.data
        ? h('div', { class: 'stack-sm' },
          ...(a.data.hypotheses || []).map(hypothesisCompact),
          a.data.mitigation ? h('div', { class: 'mitig-box' }, h('strong', null, L('止血建议：', 'Mitigation: ')), a.data.mitigation) : null,
          h('div', { class: 'row' }, btn(L('查看完整快筛证据', 'Open full triage report'), { cls: 'sm', onclick: () => APP.openDrawer('artifact', { artifactId: aid, title: L(`快筛报告 · ${p.id}`, `Triage Report · ${p.id}`) }) })))
        : (aid ? skeleton(3) : h('div', { class: 'muted' }, L('快筛工件加载中…', 'Loading triage artifact…'))),
      triageActionBar(p),
    ];
  } else if (it.kind === 'needs_human_finding') {
    const rev = ensureReview(it.rid) || S.reviews.get(it.rid);
    const pend = rev ? (rev.findings || []).filter((f) => f.falsifier?.verdict === 'needs_human' && !f.feedback) : [];
    body = rev
      ? h('div', { class: 'stack-sm' },
        pend.length ? pend.map((af) => findingRow(rev, af)) : h('div', { class: 'muted' }, L('该评审下待裁决条目已全部处理', 'All findings awaiting review have been resolved')),
        h('div', { class: 'row' }, btn(L('打开代码评审页', 'Open Reviews'), { cls: 'sm', onclick: () => APP.go('reviews', it.rid) })))
      : skeleton(3);
  } else if (s) {
    const isInteractive = isInteractivePromptSession(s);
    body = [
      isInteractive ? renderInteractivePromptBar(s, { variant: 'inbox', autoFetchPreview: true }) : null,
      !isInteractive && s.last_error ? h('div', { class: 'callout err' }, span(null, s.last_error)) : null,
      s.last_message ? h('div', { class: 'callout' }, renderMarkdown(s.last_message)) : null,
      h('div', { class: 'row' },
        btn(L('在会话对话中打开', 'Open in Workspace'), { cls: 'sm', onclick: () => APP.openSession(s.session_id) }),
        s.pane ? focusBtn(s.session_id) : null,
        (s.state === 'error' || s.last_error) ? btn(L('清除异常状态', 'Clear error state'), { cls: 'sm danger', busyKey: `ack:${s.session_id}`, onclick: () => APP.ackSessionError(s) }) : null),
      quickDispatchRow(`inb:${s.session_id}`, { type: 'session', id: s.session_id }),
    ];
  } else if (p) {
    const st = activeStage(p);
    body = [
      stageStepper(p, st?.id, null),
      h('div', { class: 'row', style: 'justify-content:flex-end' },
        cancelPipeBtn(p),
        st ? btn(L(`重试 ${stageName(st.id)}`, `Retry ${stageName(st.id)}`), { cls: 'primary sm', onclick: () => pipelineAction(p, { action: 'retry_stage', stage: st.id }, L(`已重试 ${stageName(st.id)}`, `Retried ${stageName(st.id)}`)) }) : null),
    ];
  }
  return h('article', { class: 'card' },
    h('div', { class: 'card-head' },
      h('div', { class: 'grow' },
        h('div', { class: 'card-title' }, cardTitle),
        h('div', { class: 'card-meta' },
          statusInd(dotKind, kLabel),
          p ? h('a', { class: 'mono', href: `#/pipelines/${enc(p.id)}` }, p.id) : null,
          p ? span('muted', base(p.project?.root)) : null,
          s ? [agentTag(s.agent), s.pane ? span('tag host-pfx', paneDisplayId(s)) : null] : null,
          span('tabular', L(`已等待 ${dur(Date.now() - (it.since || Date.now()))}`, `Waiting ${dur(Date.now() - (it.since || Date.now()))}`))))),
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
    ['all', L(`进行中（${activeCount}）`, `Active (${activeCount})`)],
    ['feature', L('功能协作', 'Feature Pipeline')],
    ['debug', L('两速查错', 'Two-Speed Debug')],
    ['done', L(`已结束（${doneCount}）`, `Finished (${doneCount})`)],
  ];
  return h('div', { class: 'stack' },
    h('div', { class: 'page-head' },
      h('div', null, h('h1', null, L('流水线', 'Pipelines')), h('p', null, L('Feature Pipeline 与 Two-Speed Debug 的阶段进度与产物记录', 'Stage progress and artifacts for Feature Pipelines and Two-Speed Debug'))),
      h('div', { class: 'row' },
        btn(L('发起新功能协作', 'New Feature Pipeline'), { cls: 'sm', onclick: () => { UI.modal = 'feature'; APP.schedule(); } }),
        btn(L('发起两速查错', 'New Two-Speed Debug'), { cls: 'sm', onclick: () => { UI.modal = 'debug'; APP.schedule(); } }))),
    h('div', { class: 'chips' }, filters.map(([k, l]) => h('button', {
      type: 'button',
      class: `chip${f === k ? ' on' : ''}`,
      onclick: () => { UI.pipeFilter = k; APP.schedule(); },
    }, l))),
    list.length
      ? list.map((p) => (p.playbook === 'debug' ? debugPipelineCard(p) : featurePipelineCard(p)))
      : h('div', { class: 'card' }, emptyState(
        L('当前筛选下没有流水线', 'No pipelines match this filter'),
        L('你可以在「会话对话」底部直接切换候选模式发起协作或查错，也可以点右上角按钮创建。', 'Start a Feature Pipeline or Two-Speed Debug from the composer in Workspace, or use the buttons above.'))));
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
        h('div', { class: 'card-title' }, p.goal || L('未命名功能协作', 'Untitled Feature Pipeline')),
        h('div', { class: 'card-meta' },
          h('code', null, p.id),
          statusInd(p.status),
          span('tag', L('功能协作', 'Feature')),
          span('muted', base(p.project?.root)),
          p.options?.review_depth ? span('tag soft-plum mono', p.options.review_depth) : null,
          span('tabular', L(`更新于 ${ago(p.updated_ms)}`, `Updated ${ago(p.updated_ms)}`)))),
      btn(L('流程元数据', 'Metadata'), { cls: 'ghost sm', onclick: () => APP.openDrawer('pipeline', { pipelineId: p.id }) })),
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
        st.session_id ? btn(L('打开会话', 'Open session'), { cls: 'ghost sm', onclick: () => APP.openSession(st.session_id) }) : null,
        aid ? btn(L('查看阶段工件', 'View artifact'), { cls: 'sm', onclick: () => APP.openDrawer('artifact', { artifactId: aid, title: `${stageName(st.id)} · ${p.id}` }) }) : span('muted', L('暂无工件', 'No artifact yet')))) : null,
    target && ['running', 'awaiting_human', 'blocked'].includes(st.status)
      ? quickDispatchRow(`st:${p.id}:${st.id}`, target, L(`给 ${stageName(st.id)} 阶段会话发消息…`, `Send a message to ${stageName(st.id)} session…`), p.status === 'awaiting_human' ? 'sm' : 'primary sm') : null,
    actionArea);
}

function hypothesisCompact(hy, i) {
  return h('div', { class: 'hyp-card' },
    h('div', { class: 'row', style: 'justify-content:space-between' },
      span('tag soft-amber', L(`假设 #${i + 1} · 未证实`, `Hypothesis #${i + 1} · Unconfirmed`)),
      span('muted tabular', bar(hy.confidence, hy.confidence >= 0.7 ? 'hi' : hy.confidence >= 0.4 ? 'mid' : 'low'), ` ${pct(hy.confidence)}`)),
    renderMarkdown(hy.claim),
    hy.how_to_confirm ? h('div', { class: 'muted', style: 'font-size:12.5px' }, L('验证思路：', 'How to verify: '), renderInlineMd(hy.how_to_confirm)) : null);
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
      h('div', { class: 'row' }, span('tag soft-amber', 'Phase A'), h('strong', null, L('快筛 Fast Triage', 'Fast Triage')), statusInd(tri?.status || 'pending')),
      tri?.binding?.agent ? agentTag(tri.binding.agent) : null),
    triId
      ? (triArt?.state === 'ok' && triArt.data
        ? h('div', { class: 'stack-sm' },
          ...(triArt.data.hypotheses || []).map(hypothesisCompact),
          triArt.data.mitigation ? h('div', { class: 'mitig-box' }, h('strong', null, L('止血建议：', 'Mitigation: ')), triArt.data.mitigation) : null,
          h('div', { class: 'row' }, btn(L('抽屉查看完整快筛', 'Open full triage'), { cls: 'sm', onclick: () => APP.openDrawer('artifact', { artifactId: triId, title: L(`快筛报告 · ${p.id}`, `Triage Report · ${p.id}`) }) })))
        : skeleton(3))
      : h('div', { class: 'muted' }, tri?.status === 'running' ? L('正在快速收集线索与初步假设…', 'Gathering clues and initial hypotheses…') : L('等待启动快筛', 'Waiting to start triage')),
    canDecide ? triageActionBar(p) : null);
  const phaseB = h('div', { class: 'phase' },
    h('div', { class: 'phase-head' },
      h('div', { class: 'row' }, span('tag soft-plum', 'Phase B'), h('strong', null, L('深挖 Deep Root-Cause', 'Deep Root-Cause')), statusInd(deep?.status || 'pending')),
      deep?.binding?.agent ? agentTag(deep.binding.agent) : null),
    deepId
      ? (deepArt?.state === 'ok' && deepArt.data
        ? h('div', { class: 'stack-sm' },
          h('div', { style: 'font-weight:600;color:var(--ink)' }, deepArt.data.root_cause?.claim || '—'),
          h('div', { class: 'row' },
            deepArt.data.repro_before ? statusInd(deepArt.data.repro_before.outcome, L(`修复前复现：${deepArt.data.repro_before.outcome}`, `Repro before fix: ${deepArt.data.repro_before.outcome}`), true) : null,
            deepArt.data.verification_after ? statusInd(deepArt.data.verification_after.outcome, L(`修复后验证：${deepArt.data.verification_after.outcome}`, `Verified after fix: ${deepArt.data.verification_after.outcome}`), true) : null),
          deepArt.data.ruled_out?.length ? span('muted tabular', L(`已排除 ${deepArt.data.ruled_out.length} 条假设`, `${deepArt.data.ruled_out.length} ruled out`)) : null,
          h('div', { class: 'row' }, btn(L('抽屉查看完整 RCA', 'Open full RCA'), { cls: 'sm', onclick: () => APP.openDrawer('artifact', { artifactId: deepId, title: L(`根因分析 · ${p.id}`, `Root Cause Analysis · ${p.id}`) }) })))
        : skeleton(3))
      : h('div', { class: 'muted' }, deep?.status === 'running' ? L('正在复现并验证根本原因…', 'Reproducing and verifying root cause…') : L('等待 Phase A 决策后启动', 'Waiting for Phase A decision')));
  return h('article', { class: `card${UI.arg === p.id ? ' focus-ring' : ''}`, id: `pl-${p.id}` },
    h('div', { class: 'card-head' },
      h('div', { class: 'grow' },
        h('div', { class: 'card-title' }, p.goal || L('未命名排查', 'Untitled Debug')),
        h('div', { class: 'card-meta' }, h('code', null, p.id), statusInd(p.status), span('tag soft-amber', L('两速查错', 'Two-Speed Debug')), span('muted', base(p.project?.root)), span('tabular', L(`更新于 ${ago(p.updated_ms)}`, `Updated ${ago(p.updated_ms)}`)))),
      btn(L('流程元数据', 'Metadata'), { cls: 'ghost sm', onclick: () => APP.openDrawer('pipeline', { pipelineId: p.id }) })),
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
  if (g2Of(af) === 'fabricated' || af.disposition === 'fabricated') r.push(L('G2 证据不可验证或伪造', 'G2 unverified / fabricated evidence'));
  if (g3Of(af) === 'disproved') r.push(L('G3 被 Falsifier 证伪', 'G3 disproved by Falsifier'));
  if (af.disposition === 'returned_g1') r.push(L('G1 结构退回', 'G1 returned for missing fields'));
  return r.length ? r.join(L('；', '; ')) : L('已过滤', 'Filtered');
}

export function viewReviews() {
  const revs = [...S.reviews.values()].sort((a, b) => (b.report?.header?.created_ms || 0) - (a.report?.header?.created_ms || 0));
  const dropped = revs.flatMap((r) => (r.findings || []).filter(isDropped).map((af) => [r, af]));
  const tabs = [
    ['reports', L(`评审报告（${revs.length}）`, `Reviews (${revs.length})`)],
    ['dropped', L(`Dropped Findings · 可审计（${dropped.length}）`, `Dropped Findings (${dropped.length})`)],
    ['precision', L('Precision 准确率遥测', 'Precision Telemetry')],
  ];
  let body;
  if (S.reviewsState === 'loading' && !revs.length) body = h('div', { class: 'card' }, skeleton(5));
  else if (S.reviewsState === 'err' && !revs.length) body = errorBanner(L(`评审列表加载失败：${S.reviewsErr}`, `Failed to load reviews: ${S.reviewsErr}`), L('重试加载', 'Retry'), loadReviews);
  else if (UI.revTab === 'reports') {
    body = revs.length ? h('div', { class: 'stack' }, revs.map(reviewCard)) : h('div', { class: 'card' }, emptyState(
      L('暂无代码评审报告', 'No Review Reports Yet'),
      L('当 Reviewer 提交 review.v1 报告后，会经过 G1 / G2 / G3 门禁并显示在这里。', 'Submitted review.v1 reports pass through G1 / G2 / G3 gates and appear here.')));
  } else if (UI.revTab === 'dropped') {
    body = droppedList(dropped);
  } else {
    body = precisionTable();
  }
  return h('div', { class: 'stack' },
    h('div', { class: 'page-head' },
      h('div', null, h('h1', null, L('代码评审', 'Reviews')), h('p', null, L('review.v1 反误报审计 —— G1 结构校验 · G2 证据核实 · G3 Falsifier 证伪，被过滤条目全量可审计', 'review.v1 anti-hallucination audit — G1 structure · G2 evidence verification · G3 Falsifier disproof, with full audit trail for dropped findings'))),
      btn(L('刷新评审数据', 'Refresh Reviews'), { cls: 'sm', onclick: () => { loadReviews(); if (UI.revTab === 'precision') loadPrecision(); } })),
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
        statusInd(rev.complete ? 'done' : 'pending_falsifier', rev.complete ? L('已完成', 'Complete') : L('等待 Falsifier', 'Awaiting Falsifier'), true),
        span('tag tabular', L(`G2 验证 ${cnt((f) => g2Of(f) === 'verified')} / 伪造 ${cnt((f) => g2Of(f) === 'fabricated')}`, `G2 verified ${cnt((f) => g2Of(f) === 'verified')} / fabricated ${cnt((f) => g2Of(f) === 'fabricated')}`)))),
    h('div', { class: 'stack-sm' },
      major.length ? major.map((af) => findingRow(rev, af))
        : (fs.length ? h('div', { class: 'muted' }, L('无 blocker / major 级发现', 'No blocker or major findings')) : h('div', { class: 'callout ok' }, L('未发现问题 —— Reviewer 已记录检查范围（checked_areas）。', 'No issues found — Reviewer recorded checked_areas.'))),
      minor.length ? h('details', { 'data-k': `rv:min:${rev.id}`, open: UI.open.has(`rv:min:${rev.id}`) },
        h('summary', null, L(`折叠显示 ${minor.length} 条 minor / nit（不推送飞书）`, `Show ${minor.length} minor / nit findings (not pushed to Feishu)`)),
        h('div', { class: 'stack-sm', style: 'margin-top:8px' }, minor.map((af) => findingRow(rev, af)))) : null,
      droppedCnt ? h('div', { class: 'row muted', style: 'font-size:12.5px;margin-top:4px' },
        L(`另有 ${droppedCnt} 条被门禁过滤的发现`, `${droppedCnt} findings filtered by gates`),
        btn(L('在 Dropped Findings 中审计 →', 'Audit in Dropped Findings →'), { cls: 'ghost xs', onclick: () => { UI.revTab = 'dropped'; APP.schedule(); } })) : null));
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
  }, L(`已标记为 ${lbl}`, `Marked as ${lbl}`));
  return h('div', { class: `finding-row${dropped ? ' dropped' : ''}` },
    span(`tag ${sevTagCls(sev)}`, sev),
    span('tag', f.category),
    g2 ? statusInd(g2, `G2 ${g2}`) : null,
    g3 ? statusInd(g3, `G3 ${g3 === 'pending_falsifier' ? L('待证伪', 'pending falsifier') : g3}`) : null,
    h('div', { class: 'finding-claim' }, f.claim, dropped ? h('div', { class: 'muted', style: 'font-size:12px;margin-top:2px' }, L(`过滤原因：${dropReasons(af)}`, `Drop reason: ${dropReasons(af)}`)) : null),
    h('code', null, `${f.location?.file}:${f.location?.line_start}`),
    span('muted tabular', pct(f.confidence)),
    h('div', { class: 'row' },
      btn(L('有效', 'Valid'), { cls: `xs${fb === 'valid' ? ' on' : ''}`, onclick: () => postFb(L('有效', 'Valid'), 'valid') }),
      btn(L('误报', 'False positive'), { cls: `xs${fb === 'false_positive' ? ' on' : ''}`, onclick: () => postFb(L('误报', 'False positive'), 'false_positive') }),
      btn(L('暂不修', "Won't fix"), { cls: `xs ghost${fb === 'wontfix' ? ' on' : ''}`, onclick: () => postFb(L('暂不修复', "Won't fix"), 'wontfix') }),
      btn(L('抽屉详情', 'Inspect'), { cls: 'xs', onclick: () => APP.openDrawer('finding', { reviewId: rev.id, findingId: f.id, dropped }) })));
}

function droppedList(dropped) {
  return h('div', { class: 'stack' },
    h('div', { class: 'callout' }, L('所有被 G1 / G2 / G3 门禁过滤的发现（含 disproved 与 fabricated）均在此保留可审计，绝不静默丢弃。如认为存在误杀，点开「抽屉详情」即可查看反证并申诉恢复。', 'All findings filtered by G1 / G2 / G3 gates (including disproved and fabricated) are preserved here for full auditability and never silently dropped. Click "Inspect" to review counter-evidence and reinstate.')),
    dropped.length
      ? h('div', { class: 'card stack-sm' }, dropped.map(([r, af]) => h('div', { class: 'stack-xs' },
        h('div', { class: 'row muted', style: 'font-size:12px' }, h('code', null, r.id), span('tag soft-plum mono', r.report?.depth || 'D1'), r.report?.header?.author ? agentTag(r.report.header.author.agent) : null),
        findingRow(r, af, { dropped: true }))))
      : h('div', { class: 'card' }, emptyState(L('没有被过滤的发现', 'No Dropped Findings'), L('目前所有评审条目均已正常交付或折叠。', 'All review findings so far passed gates or were collapsed as minor/nit.'))));
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
    }, a || L('全部 Agent', 'All Agents')))),
    span('muted', 'Precision = valid / (valid + false_positive)'));
  if (S.precisionState === 'loading' && !S.precision) return h('div', { class: 'card' }, skeleton(4));
  if (S.precisionState === 'err') return errorBanner(L(`准确率遥测加载失败：${S.precisionErr}`, `Failed to load precision telemetry: ${S.precisionErr}`), L('重试', 'Retry'), loadPrecision);
  if (!rows.length) return h('div', { class: 'stack' }, filterBar, h('div', { class: 'card' }, emptyState(
    L('暂无反馈遥测数据', 'No Feedback Telemetry Yet'),
    L('对评审条目点击「有效 / 误报 / 暂不修」后，将按模型与深度统计 Precision。', 'Submit Valid / False positive / Won\'t fix feedback on findings to compute Precision by model and depth.'))));
  return h('div', { class: 'stack' }, filterBar,
    h('div', { class: 'card table-wrap' }, h('table', { class: 'tbl' },
      h('thead', null, h('tr', null, ['Agent', 'Model', 'Depth', 'Category', L('交付', 'Delivered'), L('有效', 'Valid'), L('误报', 'FP'), L('暂不修', "Won't Fix"), L('伪造', 'Fabricated'), L('证伪', 'Disproved'), 'Precision'].map((x, i) => h('th', { class: i > 3 ? 'num' : '' }, x)))),
      h('tbody', null, rows.map((r) => h('tr', null,
        h('td', null, agentTag(r.agent)), h('td', { class: 'mono' }, r.model || '—'),
        h('td', null, span('tag soft-plum mono', r.depth)), h('td', null, r.category),
        ...['delivered', 'valid', 'false_positive', 'wontfix', 'fabricated', 'disproved'].map((k) => h('td', { class: 'num' }, r[k] ?? 0)),
        h('td', { class: 'num' }, r.precision == null ? span('muted', '—') : h('span', { class: 'row', style: 'justify-content:flex-end' }, bar(r.precision, r.precision < 0.5 ? 'low' : r.precision < 0.8 ? 'mid' : 'hi'), ` ${pct(r.precision)}`))))),
      h('tfoot', null, h('tr', null,
        h('td', { colspan: 4 }, L('合计', 'Total')),
        ...['delivered', 'valid', 'false_positive', 'wontfix', 'fabricated', 'disproved'].map((k) => h('td', { class: 'num' }, sum(k))),
        h('td', { class: 'num' }, v + fp ? pct(v / (v + fp)) : '—'))))));
}
