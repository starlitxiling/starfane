// starfane web console — DOM builders, formatters, and shared UI primitives
import { TOAST_MS, levelLabel } from './constants.js';
import { L, localeTag } from './i18n.js';
import { UI, APP, stageName, stateText } from './store.js';

export const $ = (s, r = document) => r.querySelector(s);

export function append(el, kids) {
  for (const k of kids) {
    if (k == null || k === false) continue;
    if (Array.isArray(k)) append(el, k);
    else if (k instanceof Node) el.appendChild(k);
    else el.appendChild(document.createTextNode(String(k)));
  }
}

export function h(tag, props, ...kids) {
  const el = document.createElement(tag);
  let value;
  if (props) {
    for (const [k, v] of Object.entries(props)) {
      if (v == null || v === false) continue;
      if (k === 'class') el.className = v;
      else if (k === 'value') value = v;
      else if (k.length > 2 && k.startsWith('on') && typeof v === 'function') el.addEventListener(k.slice(2), v);
      else if (v === true) el.setAttribute(k, '');
      else el.setAttribute(k, v);
    }
  }
  append(el, kids);
  if (value !== undefined) el.value = value;
  return el;
}

export const span = (cls, ...kids) => h('span', { class: cls }, ...kids);

export function btn(label, { cls = '', onclick, disabled, title, busyKey } = {}) {
  const isBusy = busyKey && UI.busy.has(busyKey);
  return h('button', {
    type: 'button',
    class: `btn ${cls}${isBusy ? ' is-busy' : ''}`,
    onclick,
    disabled: disabled || isBusy,
    title,
  }, label);
}

export function field(key, attrs = {}, def = '') {
  const { tag = 'input', ...rest } = attrs;
  const el = h(tag, { ...rest, 'data-draft': key });
  el.value = UI.drafts.has(key) ? UI.drafts.get(key) : def;
  el.addEventListener('input', () => UI.drafts.set(key, el.value));
  return el;
}

export function selectField(key, options, def = '', attrs = {}) {
  const cur = UI.drafts.has(key) ? UI.drafts.get(key) : def;
  const el = h('select', { class: 'inp', ...attrs, 'data-draft': key },
    options.map(([v, l]) => h('option', { value: v, selected: v === cur }, l)));
  el.value = cur;
  el.addEventListener('change', () => UI.drafts.set(key, el.value));
  return el;
}

export const pct = (x) => (typeof x === 'number' ? `${Math.round(x * 100)}%` : '—');

export function dur(ms) {
  const s = Math.max(0, Math.floor((ms || 0) / 1000));
  if (s < 60) return `${s}s`;
  const m = Math.floor(s / 60);
  if (m < 60) return `${m}m${String(s % 60).padStart(2, '0')}s`;
  const hr = Math.floor(m / 60);
  if (hr < 48) return `${hr}h${String(m % 60).padStart(2, '0')}m`;
  return `${Math.floor(hr / 24)}d`;
}

export function ago(ms) {
  if (!ms) return '—';
  const d = Math.max(0, Date.now() - ms);
  return d < 5000 ? L('刚刚', 'just now') : L(`${dur(d)}前`, `${dur(d)} ago`);
}

export function clock(ms) {
  return ms ? new Date(ms).toLocaleTimeString(localeTag(), { hour12: false }) : '';
}

export function clockHM(ms) {
  return ms ? new Date(ms).toLocaleTimeString(localeTag(), { hour12: false, hour: '2-digit', minute: '2-digit' }) : '';
}

export function fmtBytes(n) {
  if (n < 1024) return `${n} B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KB`;
  return `${(n / (1024 * 1024)).toFixed(1)} MB`;
}

export function muteRemain(until) {
  const rem = Math.max(0, (until || 0) - Date.now());
  const mins = Math.ceil(rem / 60000);
  if (mins < 1) return '<1m';
  if (mins < 60) return `${mins}m`;
  const hr = Math.floor(mins / 60);
  const m = mins % 60;
  if (hr < 24) return m ? `${hr}h${m}m` : `${hr}h`;
  const d = Math.floor(hr / 24);
  const rh = hr % 24;
  return rh ? `${d}d${rh}h` : `${d}d`;
}

export function muteUntilLabel(ms) {
  if (!ms) return '';
  const now = new Date();
  const dt = new Date(ms);
  const sameDay = now.getFullYear() === dt.getFullYear() && now.getMonth() === dt.getMonth() && now.getDate() === dt.getDate();
  const tom = new Date(now.getFullYear(), now.getMonth(), now.getDate() + 1);
  const isTom = tom.getFullYear() === dt.getFullYear() && tom.getMonth() === dt.getMonth() && tom.getDate() === dt.getDate();
  const hm = clockHM(ms);
  if (sameDay) return L(`今天 ${hm}`, `Today ${hm}`);
  if (isTom) return L(`明天 ${hm}`, `Tomorrow ${hm}`);
  return `${dt.getMonth() + 1}/${dt.getDate()} ${hm}`;
}

export function tomorrowAt9Ms() {
  const d = new Date();
  d.setDate(d.getDate() + 1);
  d.setHours(9, 0, 0, 0);
  return d.getTime();
}

export function timeStrToUntilMs(hhmm) {
  const m = String(hhmm || '').match(/^(\d{2}):(\d{2})$/);
  if (!m) return null;
  const hh = Number(m[1]);
  const mm = Number(m[2]);
  if (hh > 23 || mm > 59) return null;
  const d = new Date();
  d.setHours(hh, mm, 0, 0);
  if (d.getTime() <= Date.now() + 30000) d.setDate(d.getDate() + 1);
  return d.getTime();
}

export function defaultMuteTimeStr() {
  const d = new Date(Date.now() + 3600000);
  d.setMinutes(0, 0, 0);
  if (d.getTime() <= Date.now() + 900000) d.setHours(d.getHours() + 1);
  return `${String(d.getHours()).padStart(2, '0')}:${String(d.getMinutes()).padStart(2, '0')}`;
}

export function shorthand(b) {
  if (!b) return L('默认', 'default');
  if (b.same_as) return L(`同 ${stageName(b.same_as)}`, `same as ${stageName(b.same_as)}`);
  let s = b.agent || 'auto';
  if (b.model) s += `:${b.model}`;
  if (b.effort) s += `@${b.effort}`;
  if (b.native) s += ` /${b.native}`;
  return s;
}

export const isFin = (v) => typeof v === 'number' && Number.isFinite(v);

export function fmtTokens(n) {
  if (!isFin(n) || n < 0) return '—';
  if (n >= 1_000_000_000) return `${(n / 1_000_000_000).toFixed(n >= 10_000_000_000 ? 1 : 2).replace(/\.?0+$/, '')}B`;
  if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(n >= 10_000_000 ? 1 : 2).replace(/\.0+$/, '')}M`;
  if (n >= 1_000) return `${(n / 1_000).toFixed(n >= 100_000 ? 0 : 1).replace(/\.0$/, '')}K`;
  return String(Math.round(n));
}

export function cacheHitRate(inputTokens, cacheReadTokens) {
  if (!isFin(inputTokens) || inputTokens <= 0 || !isFin(cacheReadTokens) || cacheReadTokens < 0) return null;
  return Math.max(0, Math.min(100, (cacheReadTokens / inputTokens) * 100));
}

export function fmtHitRate(rate) {
  if (!isFin(rate) || rate < 0) return '—';
  if (rate === 0 || rate >= 99.95) return `${Math.round(rate)}%`;
  return `${rate.toFixed(1).replace(/\.0$/, '')}%`;
}

export function cacheHitTone(rate) {
  if (!isFin(rate) || rate < 0) return 'muted';
  if (rate >= 80) return 'good';
  if (rate >= 50) return 'mid';
  return 'low';
}

export function ctxPctOf(m) {
  if (!m) return null;
  if (isFin(m.context_used_pct)) return Math.max(0, Math.min(100, m.context_used_pct));
  if (isFin(m.context_used_tokens) && isFin(m.context_window_tokens) && m.context_window_tokens > 0) {
    return Math.max(0, Math.min(100, (m.context_used_tokens / m.context_window_tokens) * 100));
  }
  return null;
}

export function tpsOf(m) {
  if (!m || !isFin(m.tps) || m.tps <= 0) return null;
  return m.tps;
}

export function ttftOf(m) {
  if (!m || !isFin(m.ttft_ms) || m.ttft_ms <= 0) return null;
  return m.ttft_ms;
}

export function ttftTone(ms) {
  if (!isFin(ms) || ms <= 0) return 'ok';
  return ms < 3000 ? 'fast' : ms <= 12000 ? 'ok' : 'slow';
}

export function ctxTone(p) {
  if (p == null) return 'ok';
  if (p > 85) return 'high';
  if (p >= 70) return 'warn';
  return 'ok';
}

export function metricsSrcLabel(src) {
  return {
    agy_db: L('AGY 会话库 (原生)', 'AGY native DB'),
    transcript: L('对话日志 (原生)', 'Native transcript'),
    status_line: L('终端状态栏', 'Terminal status line'),
    cc_switch: L('cc-switch 代理日志', 'cc-switch proxy log'),
    none: L('暂无数据源', 'No telemetry source'),
  }[src] || src || L('未知', 'Unknown');
}

export function fmtMs(ms) {
  if (!isFin(ms) || ms < 0) return '—';
  return ms < 1000 ? `${Math.round(ms)} ms` : `${(ms / 1000).toFixed(2)}s`;
}

export function ctxBar(pctVal, small = false) {
  const tone = ctxTone(pctVal);
  const w = pctVal == null ? 0 : Math.max(0, Math.min(100, pctVal));
  return h('span', { class: `m-bar${small ? ' sm' : ''}${tone !== 'ok' ? ` m-${tone}` : ''}` }, h('i', { style: `width:${w.toFixed(1)}%` }));
}

export function sessionTotalTokens(m) {
  if (!m) return null;
  const hasIn = isFin(m.total_input_tokens);
  const hasOut = isFin(m.total_output_tokens);
  if (!hasIn && !hasOut) return null;
  const tot = (hasIn ? m.total_input_tokens : 0) + (hasOut ? m.total_output_tokens : 0);
  return Number.isFinite(tot) && tot > 0 ? tot : null;
}

export function buildTelemetryStrip(s) {
  const m = s?.metrics;
  const pctVal = ctxPctOf(m);
  const tpsVal = tpsOf(m);
  const ttftVal = ttftOf(m);
  const totVal = sessionTotalTokens(m);
  const tone = ctxTone(pctVal);
  const hasUsed = isFin(m?.context_used_tokens);
  const hasWin = isFin(m?.context_window_tokens) && m.context_window_tokens > 0;
  const hasCtx = pctVal != null || hasUsed;
  if (!hasCtx && tpsVal == null && ttftVal == null && totVal == null) {
    return h('div', { class: 'ws-telemetry', 'data-tel-sid': s?.session_id || '' },
      span('ws-tel-empty', L('遥测待采集', 'Telemetry pending')));
  }
  let ctxText = '';
  if (hasUsed && hasWin && pctVal != null) ctxText = `${L('上下文', 'CTX')} ${fmtTokens(m.context_used_tokens)} / ${fmtTokens(m.context_window_tokens)} · ${Math.round(pctVal)}%`;
  else if (pctVal != null) ctxText = `${L('上下文', 'CTX')} ${Math.round(pctVal)}%`;
  else if (hasUsed) ctxText = `${L('上下文', 'CTX')} ${fmtTokens(m.context_used_tokens)}`;
  const ctxTipParts = [];
  if (m?.source && m.source !== 'none') ctxTipParts.push(`${L('来源：', 'Source: ')}${metricsSrcLabel(m.source)}`);
  if (isFin(m?.total_input_tokens) || isFin(m?.total_output_tokens)) {
    ctxTipParts.push(L(`累计输入 ${fmtTokens(m?.total_input_tokens)} · 输出 ${fmtTokens(m?.total_output_tokens)}`, `Total in ${fmtTokens(m?.total_input_tokens)} · out ${fmtTokens(m?.total_output_tokens)}`));
  }
  const tpsTipParts = [];
  if (ttftVal != null) tpsTipParts.push(L(`首字延迟 TTFT：${fmtMs(ttftVal)}`, `TTFT: ${fmtMs(ttftVal)}`));
  if (isFin(m?.last_output_tokens) && isFin(m?.last_duration_ms)) {
    tpsTipParts.push(L(`最近回合：${fmtTokens(m.last_output_tokens)} tok / ${fmtMs(m.last_duration_ms)}`, `Last turn: ${fmtTokens(m.last_output_tokens)} tok / ${fmtMs(m.last_duration_ms)}`));
  }
  if (m?.source && m.source !== 'none') tpsTipParts.push(`${L('来源：', 'Source: ')}${metricsSrcLabel(m.source)}`);
  const ttftTipParts = [];
  if (ttftVal != null) {
    const turnCnt = Array.isArray(m?.recent_turns) ? m.recent_turns.length : 0;
    ttftTipParts.push(L(`最近首字延迟：${fmtMs(ttftVal)}`, `Latest TTFT: ${fmtMs(ttftVal)}`));
    if (isFin(m?.avg_ttft_ms) && m.avg_ttft_ms > 0) {
      ttftTipParts.push(L(`平均首字延迟：${fmtMs(m.avg_ttft_ms)}`, `Average TTFT: ${fmtMs(m.avg_ttft_ms)}`));
    }
    ttftTipParts.push(L(`已记录回合：${turnCnt}`, `Recorded turns: ${turnCnt}`));
    if (m?.source && m.source !== 'none') ttftTipParts.push(`${L('来源：', 'Source: ')}${metricsSrcLabel(m.source)}`);
  }
  const totHit = totVal != null && isFin(m?.total_cache_read_tokens) && m.total_cache_read_tokens > 0
    ? cacheHitRate(m?.total_input_tokens, m.total_cache_read_tokens)
    : null;
  const usageTipParts = totVal != null ? [
    L(`会话总消耗：${fmtTokens(totVal)} tokens`, `Session total: ${fmtTokens(totVal)} tokens`),
    L(`输入 ${fmtTokens(m?.total_input_tokens || 0)} · 输出 ${fmtTokens(m?.total_output_tokens || 0)}`, `In ${fmtTokens(m?.total_input_tokens || 0)} · Out ${fmtTokens(m?.total_output_tokens || 0)}`),
    ...(isFin(m?.total_cache_read_tokens) && m.total_cache_read_tokens > 0
      ? [L(
        `缓存命中 ${fmtTokens(m.total_cache_read_tokens)}${totHit != null ? `（命中率 ${fmtHitRate(totHit)}）` : ''}`,
        `Cache read ${fmtTokens(m.total_cache_read_tokens)}${totHit != null ? ` (${fmtHitRate(totHit)} hit)` : ''}`,
      )]
      : []),
    L('点击查看每日 Token 用量统计', 'Click to view daily token usage dashboard'),
  ] : [];
  const ctxCls = tone === 'high' ? ' ws-tel-high' : tone === 'warn' ? ' ws-tel-warn' : '';
  const ttftCls = ttftVal != null ? (() => {
    const t = ttftTone(ttftVal);
    return t === 'fast' ? ' ttft-fast' : t === 'slow' ? ' ttft-slow' : '';
  })() : '';
  return h('div', { class: 'ws-telemetry', 'data-tel-sid': s?.session_id || '' },
    hasCtx ? h('span', { class: `ws-tel-pill${ctxCls}`, title: ctxTipParts.join(' | ') || L('上下文容量', 'Context window') },
      ctxText, pctVal != null ? ctxBar(pctVal, false) : null) : null,
    totVal != null ? h('button', {
      type: 'button',
      class: 'ws-tel-pill ws-tel-usage',
      title: usageTipParts.join(' | '),
      onclick: () => { UI.modal = 'usage'; APP.rerender(); },
    }, L(`消耗 ${fmtTokens(totVal)}`, `Used ${fmtTokens(totVal)}`)) : null,
    tpsVal != null ? h('span', { class: 'ws-tel-pill', title: tpsTipParts.join(' | ') || L('输出速度', 'Output speed') },
      `${tpsVal.toFixed(1)} tok/s`) : null,
    ttftVal != null ? h('span', { class: `ws-tel-pill${ttftCls}`, title: ttftTipParts.join(' | ') || L('首字延迟', 'Time to first token') },
      L(`首字 ${fmtMs(ttftVal)}`, `TTFT ${fmtMs(ttftVal)}`)) : null);
}

export function errText(e) {
  if (e && typeof e === 'object' && 'code' in e && e.code && e.code !== 'unavailable') {
    return L(`${e.message}（${e.code}）`, `${e.message} (${e.code})`);
  }
  return e?.message || String(e);
}

export function deliveryText(r) {
  if (!r) return L('已发送', 'Sent');
  const lvl = levelLabel(r.level);
  const ack = r.ack === 'unverified_legacy' ? L(' · 存量会话已写入', ' · written to legacy session') : '';
  return `${lvl} · ${stateText(r.status)}${r.pane ? ` · ${r.pane}` : ''}${ack}`;
}

export function warnText(w) {
  switch (w.warning) {
    case 'same_model_reviewer': return L(`Reviewer 与实现者使用同一模型（${w.model}），存在自审偏差风险`, `Reviewer uses the same model (${w.model}) as implementer — self-review bias risk`);
    case 'native_agent_mismatch': return L(`${stageName(w.stage)}：原生模式 ${w.native} 与 agent ${w.agent} 不匹配`, `${stageName(w.stage)}: native mode ${w.native} does not match agent ${w.agent}`);
    case 'unknown_model': return L(`${stageName(w.stage)}：${w.agent} 未知模型 “${w.model}”`, `${stageName(w.stage)}: unknown model "${w.model}" for ${w.agent}`);
    case 'stage_not_in_playbook': return L(`阶段 ${stageName(w.stage)} 不属于 ${w.playbook}，已忽略`, `Stage ${stageName(w.stage)} is not part of ${w.playbook}; ignored`);
    case 'unrecognized': return L(`未识别片段：“${w.text}”`, `Unrecognized clause: "${w.text}"`);
    default: return JSON.stringify(w);
  }
}

function promoteToastPopover(box) {
  if (!box || !box.isConnected || typeof box.showPopover !== 'function') return;
  try {
    if (box.matches(':popover-open')) box.hidePopover();
    box.showPopover();
  } catch (err) {
    console.debug('toast showPopover skipped:', err);
  }
}

export function toast(msg, kind = 'ok') {
  const box = $('#toasts');
  if (!box) return;
  const dotCls = kind === 'err' ? 'dot-error' : kind === 'warn' ? 'dot-needs_permission' : 'dot-done';
  const el = h('div', { class: 'toast' }, h('i', { class: `dot ${dotCls}` }), h('span', null, msg));
  box.appendChild(el);
  while (box.children.length > 4) box.firstChild.remove();
  promoteToastPopover(box);
  setTimeout(() => {
    el.remove();
    if (box.children.length === 0 && box.isConnected && typeof box.hidePopover === 'function') {
      try {
        if (box.matches(':popover-open')) box.hidePopover();
      } catch (err) {
        console.debug('toast hidePopover skipped:', err);
      }
    }
  }, kind === 'err' ? TOAST_MS * 2 : TOAST_MS);
}

export const statusInd = (state, text, soft = false) => span(
  `status-ind${soft ? ' pill-soft' : ''}`,
  h('i', { class: `dot dot-${state}` }),
  text || stateText(state),
);
export const agentTag = (a) => span(`tag a-${a || 'x'}`, a || '?');
export const modelTag = (m) => (m ? span('tag mono', m) : null);
export const bar = (x, cls = '') => h('span', { class: `bar ${cls}` }, h('i', { style: `width:${Math.max(0, Math.min(1, x || 0)) * 100}%` }));

export function skeleton(lines = 4) {
  const widths = ['w-60', 'w-80', 'w-80', 'w-40'];
  return h('div', { class: 'skel-wrap', 'aria-busy': 'true' },
    Array.from({ length: lines }, (_, i) => h('div', { class: `skel ${widths[i % widths.length]}` })));
}

export function emptyState(title, desc, action) {
  return h('div', { class: 'empty-state' }, h('h3', null, title), desc ? h('p', null, desc) : null, action || null);
}

export function errorBanner(msg, retryLabel, onRetry) {
  return h('div', { class: 'callout err' }, h('span', null, msg), onRetry ? btn(retryLabel || L('重试', 'Retry'), { cls: 'sm', onclick: onRetry }) : null);
}

export const kvRow = (k, v) => (v == null || v === '' || (Array.isArray(v) && !v.length) ? null : [h('div', { class: 'kv-k' }, k), h('div', { class: 'kv-v' }, v)]);
export function kvGrid(pairs) {
  const kids = pairs.flatMap(([k, v]) => kvRow(k, v) || []);
  return kids.length ? h('div', { class: 'kv-grid' }, kids) : null;
}

export const codeRefList = (l) => (l && l.length ? h('ul', { class: 'ref-list' }, l.map((c) => h('li', null, h('code', null, `${c.file}:${c.line}`), c.note ? span('muted', ` — ${c.note}`) : null))) : null);
export const plainList = (l) => (l && l.length ? h('ul', { class: 'prose' }, l.map((x) => h('li', null, typeof x === 'string' ? x : x.title || JSON.stringify(x)))) : null);

export function evBlock(e) {
  const isLog = e.type === 'log';
  const loc = isLog ? `${e.path}${e.line ? `:${e.line}` : ''}` : `${e.file}:${e.line_start}${e.line_end && e.line_end !== e.line_start ? `-${e.line_end}` : ''}`;
  return h('div', { class: 'stack-xs' }, h('div', { class: 'row' }, span('tag', isLog ? 'log' : 'code'), h('code', null, loc)), h('pre', { class: 'code-block' }, e.quote || ''));
}

export const evBlocks = (l) => (l && l.length ? h('div', { class: 'stack-sm' }, l.map((e) => evBlock(e.type ? e : { ...e, type: 'code' }))) : null);

function fallbackCopy(text, done) {
  const ta = h('textarea', { readonly: true, style: 'position:fixed;top:-9999px;left:-9999px;opacity:0' });
  ta.value = text;
  document.body.appendChild(ta);
  ta.focus();
  ta.select();
  try { ta.setSelectionRange(0, text.length); } catch { /* ignore */ }
  try {
    if (document.execCommand('copy')) done();
    else toast(L('复制失败，请手动选择复制', 'Copy failed — please copy manually'), 'warn');
  } catch {
    toast(L('复制失败，请手动选择复制', 'Copy failed — please copy manually'), 'warn');
  }
  ta.remove();
}

export function copyText(text, btnEl) {
  const done = () => {
    if (!btnEl) return;
    const orig = btnEl.textContent;
    btnEl.textContent = L('已复制 ✓', 'Copied ✓');
    setTimeout(() => { btnEl.textContent = orig; }, 1600);
  };
  if (window.isSecureContext && navigator.clipboard?.writeText) {
    navigator.clipboard.writeText(text).then(done).catch(() => fallbackCopy(text, done));
  } else {
    fallbackCopy(text, done);
  }
}
