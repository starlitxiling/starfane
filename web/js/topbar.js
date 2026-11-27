// starfane web console — top navigation bar & multi-channel notification mute popover
import { VIEWS, notifierKindLabel, viewLabel } from './constants.js';
import { L, getLang } from './i18n.js';
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
import { act, loadNotifiers, muteNotifiersApi, testNotifierApi } from './api.js';
import {
  APP,
  S,
  UI,
  activeChannelMuteIds,
  alertCount,
  allPipelines,
  applyNotifiersSnapshot,
  channelMuteUntil,
  connState,
  effectiveMuteUntil,
  globalMuted,
  isFinished,
  mutedChannelCount,
  notifiersDiffSig,
  rootSessions,
  setMute,
} from './store.js';

const KIND_CLASS_SET = new Set(['feishu', 'telegram', 'discord', 'slack', 'wecom', 'qq', 'webhook', 'macos']);
const SVG_NS = 'http://www.w3.org/2000/svg';

function svgNode(tag, attrs = {}, kids = []) {
  const el = document.createElementNS(SVG_NS, tag);
  for (const [k, v] of Object.entries(attrs)) {
    if (v != null) el.setAttribute(k, String(v));
  }
  for (const c of kids) if (c) el.appendChild(c);
  return el;
}

function mobileTabIcon(v) {
  const base = {
    class: 'mnav-ico',
    viewBox: '0 0 24 24',
    width: '19',
    height: '19',
    fill: 'none',
    stroke: 'currentColor',
    'stroke-width': '1.85',
    'stroke-linecap': 'round',
    'stroke-linejoin': 'round',
    'aria-hidden': 'true',
  };
  if (v === 'workspace') {
    return svgNode('svg', base, [
      svgNode('rect', { x: 3, y: 4, width: 18, height: 15, rx: 3 }),
      svgNode('path', { d: 'M8 9.5L11 12L8 14.5' }),
      svgNode('line', { x1: 13, y1: 14.5, x2: 16.5, y2: 14.5 }),
    ]);
  }
  if (v === 'inbox') {
    return svgNode('svg', base, [
      svgNode('path', { d: 'M3.5 13L5.5 5.5H18.5L20.5 13V18.5A1.5 1.5 0 0 1 19 20H5A1.5 1.5 0 0 1 3.5 18.5V13Z' }),
      svgNode('path', { d: 'M3.5 13H8.5L10 15.5H14L15.5 13H20.5' }),
    ]);
  }
  if (v === 'pipelines') {
    return svgNode('svg', base, [
      svgNode('rect', { x: 3, y: 4.5, width: 5.5, height: 5.5, rx: 1.5 }),
      svgNode('rect', { x: 15.5, y: 4.5, width: 5.5, height: 5.5, rx: 1.5 }),
      svgNode('rect', { x: 9.25, y: 14, width: 5.5, height: 5.5, rx: 1.5 }),
      svgNode('path', { d: 'M8.5 7.25H15.5' }),
      svgNode('path', { d: 'M5.75 10V12.25A4.5 4.5 0 0 0 9.25 16.75' }),
      svgNode('path', { d: 'M18.25 10V12.25A4.5 4.5 0 0 1 14.75 16.75' }),
    ]);
  }
  return svgNode('svg', base, [
    svgNode('path', { d: 'M12 3L19 6V11.5C19 16 16 19.2 12 21C8 19.2 5 16 5 11.5V6L12 3Z' }),
    svgNode('path', { d: 'M9 12L11 14L15.5 9.5' }),
  ]);
}

function mobileViewLabel(v) {
  return {
    workspace: L('会话', 'Workspace'),
    inbox: L('待处理', 'Inbox'),
    pipelines: L('流水线', 'Pipelines'),
    reviews: L('评审', 'Reviews'),
  }[v] || viewLabel(v);
}

function safeKindClass(kind) {
  const k = String(kind || '').toLowerCase();
  return KIND_CLASS_SET.has(k) ? k : 'webhook';
}

function muteToastText(target, until) {
  const isAll = !target || target === 'all';
  if (!until) {
    return isAll
      ? L('已恢复全部渠道通知', 'All notification channels resumed')
      : L(`已恢复渠道 ${target} 通知`, `Notifications resumed for ${target}`);
  }
  const untilStr = muteUntilLabel(until);
  return isAll
    ? L(`全部通知已静音（至 ${untilStr}，Web 不受影响）`, `All notifications muted (until ${untilStr}; Web unaffected)`)
    : L(`渠道 ${target} 已静音（至 ${untilStr}，Web 不受影响）`, `Channel ${target} muted (until ${untilStr}; Web unaffected)`);
}

export function applyMuteRemote(payload, fallbackUntil = 0, opts = {}) {
  const target = String(payload?.target || 'all').trim() || 'all';
  const busyKey = opts.busyKey || 'mute';
  return act(busyKey, async () => {
    const r = await muteNotifiersApi({
      target,
      ...(payload?.minutes !== undefined ? { minutes: payload.minutes } : {}),
      ...(payload?.until_ms !== undefined ? { until_ms: payload.until_ms } : {}),
    });
    if (r && typeof r === 'object') {
      applyNotifiersSnapshot(r);
    }
    let resolvedUntil = 0;
    if (typeof r?.muted_until_ms === 'number') {
      resolvedUntil = r.muted_until_ms;
    } else if (r?.muted_until_ms === null) {
      resolvedUntil = 0;
    } else if (target === 'all') {
      resolvedUntil = typeof r?.global_mute_until_ms === 'number'
        ? r.global_mute_until_ms
        : (r?.global_mute_until_ms === null
          ? 0
          : (typeof r?.feishu_muted_until_ms === 'number'
            ? r.feishu_muted_until_ms
            : (r?.feishu_muted_until_ms === null ? 0 : fallbackUntil)));
      if (!Object.prototype.hasOwnProperty.call(r || {}, 'global_mute_until_ms')) {
        setMute(resolvedUntil);
      }
    } else {
      resolvedUntil = channelMuteUntil(target) || fallbackUntil;
    }
    return { target, until: resolvedUntil };
  }, ({ target: t, until }) => muteToastText(t, until));
}

function sendChannelTest(ch) {
  const busyKey = `notify-test:${ch.id}`;
  const kindLbl = notifierKindLabel(ch.kind);
  return act(
    busyKey,
    () => testNotifierApi(ch.id, L('starfane Web 控制台测试通知', 'starfane Web Console test notification')),
    () => L(`已向 ${ch.id}（${kindLbl}）发送测试通知`, `Test notification sent to ${ch.id} (${kindLbl})`),
  );
}

function buildChannelRow(ch, { isGlobal, now }) {
  const ownUntil = channelMuteUntil(ch.id, now);
  const effUntil = effectiveMuteUntil(ch.id, now);
  const isMuted = effUntil > 0;
  const kindLbl = notifierKindLabel(ch.kind);
  const kindCls = safeKindClass(ch.kind);

  let statusLine;
  if (isGlobal && ownUntil > S.muteUntil) {
    statusLine = L(`全局静音 · 单渠道至 ${muteUntilLabel(ownUntil)}`, `Global mute · Channel until ${muteUntilLabel(ownUntil)}`);
  } else if (isGlobal) {
    statusLine = L(`已全局静音至 ${muteUntilLabel(S.muteUntil)}`, `Globally muted until ${muteUntilLabel(S.muteUntil)}`);
  } else if (ownUntil > 0) {
    statusLine = L(`静音至 ${muteUntilLabel(ownUntil)}（${muteRemain(ownUntil)}）`, `Muted until ${muteUntilLabel(ownUntil)} (${muteRemain(ownUntil)})`);
  } else {
    statusLine = L('开启', 'Active');
  }

  const unmuteBtn = (!isGlobal && ownUntil > 0)
    ? btn(L('取消静音', 'Unmute'), {
      cls: 'xs ghost mute-ch-unmute',
      busyKey: `mute:${ch.id}`,
      title: L(`恢复 ${ch.id} 通知`, `Unmute ${ch.id}`),
      onclick: () => applyMuteRemote({ target: ch.id, minutes: 0 }, 0, { busyKey: `mute:${ch.id}` }),
    })
    : null;

  const testBtn = btn(L('测试', 'Test'), {
    cls: 'xs mute-ch-test',
    busyKey: `notify-test:${ch.id}`,
    title: L(`向 ${ch.id} 发送测试通知`, `Send test notification to ${ch.id}`),
    onclick: () => sendChannelTest(ch),
  });

  return h('div', { class: `mute-ch-item${isMuted ? ' is-muted' : ''}` },
    h('div', { class: 'mute-ch-main' },
      h('div', { class: 'mute-ch-top' },
        h('i', { class: `dot ${isMuted ? 'dot-needs_permission' : 'dot-live'}` }),
        h('span', { class: 'mute-ch-id trunc', title: ch.id }, ch.id),
        span(`tag mute-kind mute-kind-${kindCls}`, kindLbl)),
      h('div', { class: 'mute-ch-state tabular' }, statusLine)),
    h('div', { class: 'mute-ch-acts' }, unmuteBtn, testBtn));
}

export function buildMutePopover() {
  const now = Date.now();
  const isGlobal = globalMuted(now);
  const activeChIds = activeChannelMuteIds(now);
  const channels = S.notifiers || [];
  const hasChannels = channels.length > 0;
  const currentTarget = hasChannels && channels.some((c) => c.id === UI.muteTarget) ? UI.muteTarget : 'all';
  if (UI.muteTarget !== currentTarget) UI.muteTarget = currentTarget;
  const getTarget = () => (hasChannels ? (UI.muteTarget || 'all') : 'all');

  if (!UI.muteTimeVal) UI.muteTimeVal = defaultMuteTimeStr();

  const header = h('div', { class: 'mute-head' },
    h('div', { class: 'mute-title' },
      span(null, L('通知与静音设置', 'Notifications & Mute')),
      span('tag tabular', String(channels.length))),
    h('button', {
      type: 'button',
      class: 'mute-close',
      'aria-label': L('关闭通知设置', 'Close notification settings'),
      title: L('关闭', 'Close'),
      onclick: () => { UI.muteMenu = false; APP.rerender(); },
    }, '×'));

  let statusBanner = null;
  if (isGlobal || activeChIds.length > 0) {
    const bannerMain = isGlobal
      ? L(`全部渠道已静音至 ${muteUntilLabel(S.muteUntil)}`, `All channels muted until ${muteUntilLabel(S.muteUntil)}`)
      : L(`${activeChIds.length} 个渠道已静音`, `${activeChIds.length} channel${activeChIds.length === 1 ? '' : 's'} muted`);
    const bannerSub = isGlobal && activeChIds.length > 0
      ? L(`另含 ${activeChIds.length} 个单渠道静音设置`, `Plus ${activeChIds.length} per-channel mute${activeChIds.length === 1 ? '' : 's'}`)
      : (isGlobal
        ? L(`剩余 ${muteRemain(S.muteUntil)} · Web 控制台不受影响`, `${muteRemain(S.muteUntil)} left · Web console unaffected`)
        : L('点击下方渠道可单独恢复，或一键恢复全部', 'Unmute individually below or resume all channels'));
    const unmuteLabel = (!isGlobal || activeChIds.length > 0)
      ? L('取消全部静音', 'Unmute all')
      : L('立即取消静音', 'Unmute now');
    statusBanner = h('div', { class: 'mute-status' },
      h('div', { class: 'mute-status-text' },
        h('span', { class: 'tabular' }, bannerMain),
        h('span', { class: 'mute-status-sub tabular' }, bannerSub)),
      btn(unmuteLabel, {
        cls: 'xs',
        busyKey: 'mute',
        onclick: () => applyMuteRemote({ target: 'all', minutes: 0 }, 0),
      }));
  }

  const orderedChannels = [...channels].sort((a, b) => {
    const am = channelMuteUntil(a.id, now) > 0 ? 0 : 1;
    const bm = channelMuteUntil(b.id, now) > 0 ? 0 : 1;
    return am - bm || a.id.localeCompare(b.id);
  });

  const channelsSection = h('div', { class: 'mute-sec' },
    h('div', { class: 'mute-sec-head' },
      span('mute-lbl', L('已配置渠道', 'Configured channels')),
      hasChannels ? span('mute-hint', L('测试不受静音影响', 'Test bypasses mute')) : null),
    hasChannels
      ? h('div', { class: 'mute-ch-list' }, orderedChannels.map((ch) => buildChannelRow(ch, { isGlobal, now })))
      : h('div', { class: 'mute-empty' },
        L('暂未配置外部通知渠道。可在 ', 'No notification channels configured yet. Add '),
        h('code', null, 'config.toml'),
        L(' 的 ', ' entries under '),
        h('code', null, '[[notifiers]]'),
        L(' 中配置飞书、Telegram、Discord、Slack、企微、QQ、Webhook 或 macOS 通知。', ' to enable Feishu, Telegram, Discord, Slack, WeCom, QQ, Webhook, or macOS notifications.')));

  const targetSec = hasChannels
    ? h('div', { class: 'mute-sec' },
      span('mute-lbl', L('静音范围', 'Mute scope')),
      h('select', {
        class: 'inp sm mute-target',
        'aria-label': L('静音目标范围', 'Mute target scope'),
        value: currentTarget,
        onchange: (e) => { UI.muteTarget = e.target.value || 'all'; APP.schedule(); },
      },
      h('option', { value: 'all', selected: currentTarget === 'all' }, L('全部通知渠道', 'All channels')),
      ...channels.map((ch) => h(
        'option',
        { value: ch.id, selected: currentTarget === ch.id },
        `${ch.id} (${notifierKindLabel(ch.kind)})`,
      ))))
    : null;

  const numInp = h('input', {
    type: 'number',
    min: '1',
    max: '10080',
    class: 'inp sm tabular mute-num',
    value: UI.muteCustomVal,
    'aria-label': L('自定义静音时长', 'Custom mute duration'),
    oninput: (e) => { UI.muteCustomVal = e.target.value; },
  });
  const unitSel = h('select', {
    class: 'inp sm mute-unit',
    'aria-label': L('时长单位', 'Duration unit'),
    value: UI.muteCustomUnit,
    onchange: (e) => { UI.muteCustomUnit = e.target.value; },
  },
  h('option', { value: 'm', selected: UI.muteCustomUnit === 'm' }, L('分钟', 'min')),
  h('option', { value: 'h', selected: UI.muteCustomUnit === 'h' }, L('小时', 'hr')));

  const submitCustom = () => {
    const raw = Number(numInp.value);
    if (!Number.isFinite(raw) || raw <= 0) {
      toast(L('请输入大于 0 的时长', 'Enter a duration greater than 0'), 'warn');
      return;
    }
    const mins = unitSel.value === 'h' ? Math.round(raw * 60) : Math.round(raw);
    if (mins < 1 || mins > 10080) {
      toast(L('静音时长须在 1 分钟至 7 天之间', 'Mute duration must be between 1 minute and 7 days'), 'warn');
      return;
    }
    applyMuteRemote({ target: getTarget(), minutes: mins }, Date.now() + mins * 60000);
  };
  numInp.addEventListener('keydown', (e) => {
    if (e.key === 'Enter') { e.preventDefault(); submitCustom(); }
  });

  const timeHint = span('dim tabular', '');
  const syncTimeHint = () => {
    const ts = timeStrToUntilMs(UI.muteTimeVal);
    timeHint.textContent = ts
      ? L(`将静音至 ${muteUntilLabel(ts)}`, `Will mute until ${muteUntilLabel(ts)}`)
      : L('请选择有效时间', 'Choose a valid time');
  };
  const timeInp = h('input', {
    type: 'time',
    class: 'inp sm tabular mute-time',
    value: UI.muteTimeVal,
    'aria-label': L('静音截止时刻', 'Mute until time'),
    oninput: (e) => { UI.muteTimeVal = e.target.value; syncTimeHint(); },
  });
  syncTimeHint();
  const submitTime = () => {
    const ts = timeStrToUntilMs(timeInp.value);
    if (!ts) {
      toast(L('请选择有效的截止时刻', 'Choose a valid cutoff time'), 'warn');
      return;
    }
    applyMuteRemote({ target: getTarget(), until_ms: ts }, ts);
  };

  const muteBusy = UI.busy.has('mute');
  const presets = [
    [L('15 分钟', '15 min'), () => applyMuteRemote({ target: getTarget(), minutes: 15 }, Date.now() + 900000)],
    [L('30 分钟', '30 min'), () => applyMuteRemote({ target: getTarget(), minutes: 30 }, Date.now() + 1800000)],
    [L('1 小时', '1 hour'), () => applyMuteRemote({ target: getTarget(), minutes: 60 }, Date.now() + 3600000)],
    [L('2 小时', '2 hours'), () => applyMuteRemote({ target: getTarget(), minutes: 120 }, Date.now() + 7200000)],
    [L('至明早 09:00', 'Until tomorrow 09:00'), () => {
      const ts = tomorrowAt9Ms();
      applyMuteRemote({ target: getTarget(), until_ms: ts }, ts);
    }],
  ];

  return h('div', {
    class: 'mute-pop',
    role: 'dialog',
    'aria-label': L('通知静音与渠道设置', 'Notification mute & channel settings'),
  },
  h('div', { class: 'sheet-handle', 'aria-hidden': 'true' }),
  header,
  statusBanner,
  channelsSection,
  h('div', { class: 'mute-sep' }),
  targetSec,
  h('div', { class: 'mute-sec' },
    span('mute-lbl', L('快捷静音', 'Quick mute')),
    h('div', { class: 'mute-presets' }, presets.map(([l, fn]) => h('button', {
      type: 'button',
      class: 'mute-preset',
      disabled: muteBusy,
      onclick: fn,
    }, l)))),
  h('div', { class: 'mute-sec' },
    span('mute-lbl', L('自定义时长', 'Custom duration')),
    h('div', { class: 'mute-row' }, numInp, unitSel, btn(L('应用静音', 'Apply'), { cls: 'primary sm', busyKey: 'mute', onclick: submitCustom }))),
  h('div', { class: 'mute-sec' },
    h('div', { class: 'row', style: 'justify-content:space-between' }, span('mute-lbl', L('指定截止时刻', 'Until time')), timeHint),
    h('div', { class: 'mute-row' }, timeInp, btn(L('静音至此', 'Mute until'), { cls: 'sm', busyKey: 'mute', onclick: submitTime }))));
}

function buildTopbarPillState(now) {
  const channels = S.notifiers || [];
  const count = channels.length;
  const isGlobal = globalMuted(now);
  const partialIds = activeChannelMuteIds(now);
  const partialCount = mutedChannelCount(now);

  if (isGlobal) {
    return {
      cls: 'btn sm pill on mute-pill-muted',
      title: L(
        `全部通知已静音至 ${muteUntilLabel(S.muteUntil)}`,
        `All notifications muted until ${muteUntilLabel(S.muteUntil)}`,
      ),
      kids: [
        h('i', { class: 'dot dot-needs_permission' }),
        L(`通知已静音 · ${muteRemain(S.muteUntil)}`, `Muted · ${muteRemain(S.muteUntil)}`),
      ],
    };
  }
  if (partialCount > 0) {
    const tipList = partialIds
      .map((id) => `${id} (${muteUntilLabel(channelMuteUntil(id, now))})`)
      .join(', ');
    return {
      cls: 'btn sm pill on mute-pill-partial',
      title: L(`部分通知渠道已静音：${tipList}`, `Channels muted: ${tipList}`),
      kids: [
        h('i', { class: 'dot dot-needs_permission' }),
        L(`通知 · ${partialCount} 渠道静音`, `Notify · ${partialCount} muted`),
      ],
    };
  }
  if (S.notifiersLoaded && count === 0) {
    return {
      cls: 'btn sm pill mute-pill-none',
      title: L('未配置外部通知渠道，点击查看配置说明', 'No external notification channels configured; click for setup info'),
      kids: [
        h('i', { class: 'dot dot-idle' }),
        L('通知：未配置', 'Notify: None'),
      ],
    };
  }
  const activeText = count > 1
    ? L(`通知：开启 (${count})`, `Notify: On (${count})`)
    : L('通知：开启', 'Notify: On');
  return {
    cls: 'btn sm pill',
    title: count > 0
      ? L(`已启用 ${count} 个通知渠道，点击设置静音或发送测试`, `${count} notification channel${count === 1 ? '' : 's'} active; click to mute or test`)
      : L('打开通知静音与渠道设置', 'Open notification mute & channel settings'),
    kids: [
      ...(count > 0 ? [h('i', { class: 'dot dot-live' })] : []),
      activeText,
    ],
  };
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
  const connTxt = { live: L('实时已连接', 'Live'), connecting: L('重连中…', 'Reconnecting…'), down: L('连接已断开', 'Disconnected') }[cs];
  const connTip = `starfane ${S.version || '?'} · SSE ${S.conn} · seq ${S.lastSeq}${S.uptimeMs != null ? ` · ${L('运行', 'up')} ${dur(S.uptimeMs)}` : ''}`;
  const bar = $('#topbar');
  let TOP = UI.topShell;
  if (!TOP || TOP.bar !== bar) {
    const nav = h('nav', { class: 'nav-seg', role: 'tablist', 'aria-label': L('主导航', 'Primary navigation') });
    const connPill = h('span', { class: 'status-ind pill-soft' });
    const muteBtn = h('button', {
      type: 'button',
      class: 'btn sm pill',
      'aria-haspopup': 'dialog',
      'aria-expanded': 'false',
      onclick: () => {
        UI.muteMenu = !UI.muteMenu;
        if (UI.muteMenu) loadNotifiers({ silent: true });
        APP.rerender();
      },
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
    const mobileNav = $('#mobile-nav');
    TOP = { bar, nav, mobileNav, connPill, muteBtn, mutePopSlot, popKey: '' };
    UI.topShell = TOP;
  }
  TOP.nav.replaceChildren(...VIEWS.map((v, i) => h('a', {
    class: `nav-tab${UI.view === v ? ' on' : ''}`,
    href: `#/${v}`,
    role: 'tab',
    title: L(`${viewLabel(v)}（快捷键 ${i + 1}）`, `${viewLabel(v)} (Shortcut ${i + 1})`),
  }, viewLabel(v), counts[v] ? span(`nav-count${v === 'inbox' ? ' attn' : ''}`, counts[v]) : null)));
  if (TOP.mobileNav) {
    TOP.mobileNav.setAttribute('role', 'tablist');
    TOP.mobileNav.setAttribute('aria-label', L('主导航', 'Primary navigation'));
    TOP.mobileNav.replaceChildren(...VIEWS.map((v) => h('a', {
      class: `mnav-tab${UI.view === v ? ' on' : ''}`,
      href: `#/${v}`,
      role: 'tab',
      'aria-selected': UI.view === v ? 'true' : 'false',
      'aria-label': viewLabel(v),
    },
    h('span', { class: 'mnav-ico-wrap' },
      mobileTabIcon(v),
      counts[v] ? span(`mnav-badge${v === 'inbox' ? ' attn' : ''}`, counts[v] > 99 ? '99+' : String(counts[v])) : null),
    span('mnav-lbl', mobileViewLabel(v)))));
  }
  TOP.connPill.className = `status-ind pill-soft conn-pill is-${cs}`;
  TOP.connPill.title = connTip;
  TOP.connPill.setAttribute('aria-label', connTxt);
  TOP.connPill.replaceChildren(h('i', { class: `dot dot-${cs}` }), span('conn-label', connTxt));

  const now = Date.now();
  const pill = buildTopbarPillState(now);
  TOP.muteBtn.title = pill.title;
  TOP.muteBtn.className = pill.cls;
  TOP.muteBtn.setAttribute('aria-expanded', UI.muteMenu ? 'true' : 'false');
  TOP.muteBtn.replaceChildren(...pill.kids);
  TOP.bar.classList.toggle('has-mute-open', Boolean(UI.muteMenu));

  const busySig = [...UI.busy]
    .filter((k) => k === 'mute' || k.startsWith('mute:') || k.startsWith('notify-test:'))
    .sort()
    .join(',');
  const activeMask = `${globalMuted(now) ? 1 : 0}:${activeChannelMuteIds(now).join(',')}`;
  const popKey = UI.muteMenu
    ? `open:${getLang()}:${notifiersDiffSig()}:${activeMask}:${UI.muteTarget}:${busySig}`
    : 'closed';
  if (TOP.popKey !== popKey) {
    TOP.popKey = popKey;
    if (UI.muteMenu) {
      TOP.mutePopSlot.replaceChildren(
        h('div', {
          class: 'mute-backdrop',
          'aria-hidden': 'true',
          onclick: () => { UI.muteMenu = false; APP.rerender(); },
        }),
        buildMutePopover(),
      );
    } else {
      TOP.mutePopSlot.replaceChildren();
    }
  }
}
