// starfane web console — top navigation bar & Feishu mute popover
import { VIEWS, VIEW_LABEL } from './constants.js';
import {
  $,
  btn,
  defaultMuteTimeStr,
  dur,
  h,
  muteRemain,
  muteUntilLabel,
  span,
  timeStrToUntilMs,
  toast,
  tomorrowAt9Ms,
} from './dom.js';
import { ApiError, act, api } from './api.js';
import {
  APP,
  S,
  UI,
  alertCount,
  allPipelines,
  connState,
  isFinished,
  muted,
  rootSessions,
  setMute,
} from './store.js';

export function applyMuteRemote(payload, label, fallbackUntil) {
  return act('mute', async () => {
    let r;
    try {
      r = await api('POST', '/api/feishu/mute', payload);
    } catch (e) {
      if (e instanceof ApiError && (e.status === 404 || e.status === 405)) {
        throw new ApiError('unavailable', '当前 daemon 未启用 Web 静音接口，可在飞书发送「静音 30m」', e.status);
      }
      throw e;
    }
    const until = typeof r?.feishu_muted_until_ms === 'number'
      ? r.feishu_muted_until_ms
      : (r?.feishu_muted_until_ms === null ? 0 : fallbackUntil);
    setMute(until);
    UI.muteMenu = false;
    return until;
  }, (until) => (until ? `${label}（至 ${muteUntilLabel(until)}，Web 不受影响）` : '已恢复飞书通知'));
}

export function buildMutePopover() {
  const m = muted();
  if (!UI.muteTimeVal) UI.muteTimeVal = defaultMuteTimeStr();
  const numInp = h('input', {
    type: 'number',
    min: '1',
    max: '10080',
    class: 'inp sm tabular mute-num',
    value: UI.muteCustomVal,
    'aria-label': '自定义静音时长',
    oninput: (e) => { UI.muteCustomVal = e.target.value; },
  });
  const unitSel = h('select', {
    class: 'inp sm mute-unit',
    'aria-label': '时长单位',
    value: UI.muteCustomUnit,
    onchange: (e) => { UI.muteCustomUnit = e.target.value; },
  },
  h('option', { value: 'm', selected: UI.muteCustomUnit === 'm' }, '分钟'),
  h('option', { value: 'h', selected: UI.muteCustomUnit === 'h' }, '小时'));

  const submitCustom = () => {
    const raw = Number(numInp.value);
    if (!Number.isFinite(raw) || raw <= 0) { toast('请输入大于 0 的时长', 'warn'); return; }
    const mins = unitSel.value === 'h' ? Math.round(raw * 60) : Math.round(raw);
    if (mins < 1 || mins > 10080) { toast('静音时长须在 1 分钟至 7 天之间', 'warn'); return; }
    const lbl = unitSel.value === 'h' ? `飞书通知已静音 ${raw} 小时` : `飞书通知已静音 ${mins} 分钟`;
    applyMuteRemote({ minutes: mins }, lbl, Date.now() + mins * 60000);
  };
  numInp.addEventListener('keydown', (e) => { if (e.key === 'Enter') { e.preventDefault(); submitCustom(); } });

  const timeHint = span('dim tabular', '');
  const syncTimeHint = () => {
    const ts = timeStrToUntilMs(UI.muteTimeVal);
    timeHint.textContent = ts ? `将静音至 ${muteUntilLabel(ts)}` : '请选择有效时间';
  };
  const timeInp = h('input', {
    type: 'time',
    class: 'inp sm tabular mute-time',
    value: UI.muteTimeVal,
    'aria-label': '静音截止时刻',
    oninput: (e) => { UI.muteTimeVal = e.target.value; syncTimeHint(); },
  });
  syncTimeHint();
  const submitTime = () => {
    const ts = timeStrToUntilMs(timeInp.value);
    if (!ts) { toast('请选择有效的截止时刻', 'warn'); return; }
    applyMuteRemote({ until_ms: ts }, '飞书通知已静音', ts);
  };
  const presets = [
    ['15 分钟', () => applyMuteRemote({ minutes: 15 }, '飞书通知已静音 15 分钟', Date.now() + 900000)],
    ['30 分钟', () => applyMuteRemote({ minutes: 30 }, '飞书通知已静音 30 分钟', Date.now() + 1800000)],
    ['1 小时', () => applyMuteRemote({ minutes: 60 }, '飞书通知已静音 1 小时', Date.now() + 3600000)],
    ['2 小时', () => applyMuteRemote({ minutes: 120 }, '飞书通知已静音 2 小时', Date.now() + 7200000)],
    ['至明早 09:00', () => { const ts = tomorrowAt9Ms(); applyMuteRemote({ until_ms: ts }, '飞书通知已静音', ts); }],
  ];
  return h('div', { class: 'mute-pop', role: 'dialog', 'aria-label': '飞书通知静音设置' },
    m ? h('div', { class: 'mute-status' },
      h('span', { class: 'tabular' }, `已静音至 ${muteUntilLabel(S.muteUntil)}`),
      btn('立即取消静音', { cls: 'xs', busyKey: 'mute', onclick: () => applyMuteRemote({ minutes: 0 }, '', 0) })) : null,
    h('div', { class: 'mute-sec' },
      span('mute-lbl', '快捷静音'),
      h('div', { class: 'mute-presets' }, presets.map(([l, fn]) => h('button', { type: 'button', class: 'mute-preset', onclick: fn }, l)))),
    h('div', { class: 'mute-sec' },
      span('mute-lbl', '自定义时长'),
      h('div', { class: 'mute-row' }, numInp, unitSel, btn('应用静音', { cls: 'primary sm', busyKey: 'mute', onclick: submitCustom }))),
    h('div', { class: 'mute-sec' },
      h('div', { class: 'row', style: 'justify-content:space-between' }, span('mute-lbl', '指定截止时刻'), timeHint),
      h('div', { class: 'mute-row' }, timeInp, btn('静音至此', { cls: 'sm', busyKey: 'mute', onclick: submitTime }))),
    h('div', { class: 'mute-sep' }),
    h('button', {
      type: 'button',
      class: 'mute-foot-btn',
      onclick: () => {
        UI.muteMenu = false;
        APP.schedule();
        act('feishu-test', () => api('POST', '/api/feishu/test', { text: 'starfane Web 控制台测试通知' }), '测试卡片已发送');
      },
    }, '发送飞书测试卡片'));
}

export function renderTop() {
  const cs = connState();
  const liveSessions = rootSessions().filter((s) => !s.is_subagent && s.state !== 'ended' && !!s.pane).length;
  const alerts = alertCount();
  const activePipes = allPipelines().filter((p) => !isFinished(p)).length;
  const pendingVerdicts = [...S.reviews.values()].reduce(
    (n, r) => n + (r.findings || []).filter((f) => f.falsifier?.verdict === 'needs_human' && !f.feedback).length,
    0,
  );
  const counts = { workspace: liveSessions, inbox: alerts, pipelines: activePipes, reviews: pendingVerdicts };
  const connTxt = { live: '实时已连接', connecting: '重连中…', down: '连接已断开' }[cs];
  const connTip = `starfane ${S.version || '?'} · SSE ${S.conn} · seq ${S.lastSeq}${S.uptimeMs != null ? ` · 运行 ${dur(S.uptimeMs)}` : ''}`;
  const bar = $('#topbar');
  let TOP = UI.topShell;
  if (!TOP || TOP.bar !== bar) {
    const nav = h('nav', { class: 'nav-seg', role: 'tablist', 'aria-label': '主导航' });
    const connPill = h('span', { class: 'status-ind pill-soft' });
    const muteBtn = h('button', {
      type: 'button',
      class: 'btn sm pill',
      onclick: () => { UI.muteMenu = !UI.muteMenu; APP.rerender(); },
    });
    const mutePopSlot = h('div');
    const muteWrap = h('div', { class: 'mute-wrap' }, muteBtn, mutePopSlot);
    bar.replaceChildren(
      h('a', { class: 'brand', href: '#/workspace' },
        h('img', { class: 'logo', src: '/assets/logo.svg', alt: '', width: 26, height: 26 }),
        h('span', null, 'starfane')),
      nav,
      h('div', { class: 'top-tools' }, connPill, muteWrap),
    );
    TOP = { bar, nav, connPill, muteBtn, mutePopSlot, popKey: '' };
    UI.topShell = TOP;
  }
  TOP.nav.replaceChildren(...VIEWS.map((v, i) => h('a', {
    class: `nav-tab${UI.view === v ? ' on' : ''}`,
    href: `#/${v}`,
    role: 'tab',
    title: `${VIEW_LABEL[v]}（快捷键 ${i + 1}）`,
  }, VIEW_LABEL[v], counts[v] ? span(`nav-count${v === 'inbox' ? ' attn' : ''}`, counts[v]) : null)));
  TOP.connPill.title = connTip;
  TOP.connPill.replaceChildren(h('i', { class: `dot dot-${cs}` }), connTxt);
  const m = muted();
  TOP.muteBtn.title = m ? `飞书通知已静音至 ${muteUntilLabel(S.muteUntil)}` : '设置飞书通知静音';
  TOP.muteBtn.className = `btn sm pill${m ? ' on' : ''}`;
  TOP.muteBtn.replaceChildren(...(m
    ? [h('i', { class: 'dot dot-needs_permission' }), `已静音 · ${muteRemain(S.muteUntil)}`]
    : ['飞书通知：开启']));
  const popKey = UI.muteMenu ? `open:${m ? S.muteUntil : 0}` : 'closed';
  if (TOP.popKey !== popKey) {
    TOP.popKey = popKey;
    if (UI.muteMenu) TOP.mutePopSlot.replaceChildren(buildMutePopover());
    else TOP.mutePopSlot.replaceChildren();
  }
}
