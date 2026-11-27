// starfane web console — zero-dependency safe Markdown renderer
import { CODE_COLLAPSE_LINES } from './constants.js';
import { copyText, h, span } from './dom.js';
import { L } from './i18n.js';

export function isSafeUrl(url) {
  const u = String(url || '').trim();
  return /^(https?:\/\/|file:\/\/)/i.test(u);
}

function splitBareUrlTrailing(raw) {
  let url = String(raw || '');
  let trailing = '';
  while (url.length) {
    const ch = url[url.length - 1];
    if (ch === '.' || ch === ',' || ch === ';' || ch === ':' || ch === '!' || ch === '?' || ch === '\'' || ch === '"') {
      trailing = ch + trailing;
      url = url.slice(0, -1);
      continue;
    }
    if (ch === ')' || ch === ']' || ch === '}') {
      const openCh = ch === ')' ? '(' : ch === ']' ? '[' : '{';
      let opens = 0;
      let closes = 0;
      for (let k = 0; k < url.length; k++) {
        if (url[k] === openCh) opens++;
        else if (url[k] === ch) closes++;
      }
      if (closes > opens) {
        trailing = ch + trailing;
        url = url.slice(0, -1);
        continue;
      }
    }
    break;
  }
  return [url, trailing];
}

const INLINE_RE_SRC = '`([^`\\n]+)`|\\[((?:[^\\]\\n`]|`[^`\\n]+`)+)\\]\\(([^)\\s]+)\\)|\\*\\*((?:[^*\\n`]|\\*(?!\\*)|`[^`\\n]+`)+?)\\*\\*|__([^_\\n`]+?)__|~~((?:[^~\\n`]|~(?!~)|`[^`\\n]+`)+?)~~|\\*((?:[^*\\n`]|`[^`\\n]+`)+?)\\*|<((?:https?:\\/\\/)[^\\s<>]+)>|\\b(https?:\\/\\/[^\\s<>\'"`\\u3000-\\u303F\\uFF01-\\uFF0F\\uFF1A-\\uFF20\\uFF3B-\\uFF40\\uFF5B-\\uFF65]+)|(<[bB][rR]\\s*\\/?>)|<(b|strong|i|em|u|s|del|code|sub|sup)>((?:(?!<\\/\\11>)[^\\n])+?)<\\/\\11>';
const INLINE_RE_POOL = Array.from({ length: 5 }, () => new RegExp(INLINE_RE_SRC, 'g'));
const INLINE_HINT_RE = /[`*_[~<]|https?:\/\//;
const INLINE_HTML_TAG_MAP = { b: 'strong', strong: 'strong', i: 'em', em: 'em', u: 'u', s: 'del', del: 'del', sub: 'sub', sup: 'sup' };

export function renderInlineMd(text, depth = 0, inLink = false) {
  const out = [];
  const s = String(text || '');
  if (!s || depth > 4 || !INLINE_HINT_RE.test(s)) return s ? [s] : out;
  const re = INLINE_RE_POOL[depth];
  re.lastIndex = 0;
  let last = 0;
  let m;
  while ((m = re.exec(s)) !== null) {
    if (m.index > last) out.push(s.slice(last, m.index));
    if (m[1] !== undefined) {
      out.push(h('code', { class: 'md-inline-code' }, m[1]));
    } else if (m[2] !== undefined) {
      const inner = renderInlineMd(m[2], depth + 1, true);
      if (!inLink && isSafeUrl(m[3])) {
        const isHttp = /^https?:\/\//i.test(m[3]);
        out.push(h('a', { href: m[3], ...(isHttp ? { target: '_blank', rel: 'noopener noreferrer', class: 'md-link' } : {}) }, inner));
      } else {
        out.push(...inner);
      }
    } else if (m[4] !== undefined || m[5] !== undefined) {
      out.push(h('strong', null, renderInlineMd(m[4] ?? m[5], depth + 1, inLink)));
    } else if (m[6] !== undefined) {
      out.push(h('del', null, renderInlineMd(m[6], depth + 1, inLink)));
    } else if (m[7] !== undefined) {
      out.push(h('em', null, renderInlineMd(m[7], depth + 1, inLink)));
    } else if (m[8] !== undefined || m[9] !== undefined) {
      const raw = m[8] ?? m[9];
      const [cleanUrl, trailing] = m[9] !== undefined ? splitBareUrlTrailing(raw) : [raw, ''];
      if (!inLink && isSafeUrl(cleanUrl) && /^https?:\/\/[^\s/?#]+/i.test(cleanUrl)) {
        out.push(h('a', { href: cleanUrl, target: '_blank', rel: 'noopener noreferrer', class: 'md-link' }, cleanUrl));
        if (trailing) out.push(trailing);
      } else {
        out.push(m[0]);
      }
    } else if (m[10] !== undefined) {
      out.push(h('br'));
    } else if (m[11] !== undefined) {
      if (m[11] === 'code') {
        out.push(h('code', { class: 'md-inline-code' }, m[12]));
      } else {
        const tag = INLINE_HTML_TAG_MAP[m[11]] || m[11];
        out.push(h(tag, null, renderInlineMd(m[12], depth + 1, inLink)));
      }
    }
    last = re.lastIndex;
  }
  if (last < s.length) out.push(s.slice(last));
  return out;
}

function isClosingFence(line, fenceLen) {
  const t = line.trim();
  if (t.length < fenceLen) return false;
  for (let i = 0; i < t.length; i++) if (t.charCodeAt(i) !== 96) return false;
  return true;
}

function splitTableRow(line) {
  let s = line.trim();
  if (s.startsWith('|')) s = s.slice(1);
  if (s.endsWith('|')) s = s.slice(0, -1);
  return s.split('|').map((c) => c.trim());
}

function isTableSep(line) {
  return /^\s*\|?\s*:?-{3,}:?\s*(\|\s*:?-{3,}:?\s*)+\|?\s*$/.test(line || '');
}

function tableAligns(line) {
  return splitTableRow(line).map((c) => (c.startsWith(':') && c.endsWith(':') ? 'center' : c.endsWith(':') ? 'right' : 'left'));
}

export function renderCodeBlock(lang, codeText) {
  const lineCount = codeText ? codeText.split('\n').length : 0;
  const collapsible = lineCount > CODE_COLLAPSE_LINES;
  const copyBtn = h('button', { type: 'button', class: 'md-copy-btn', onclick: (e) => copyText(codeText, e.currentTarget) }, L('复制代码', 'Copy'));
  const wrap = h('div', { class: `md-code-wrap${collapsible ? ' is-collapsed' : ''}` },
    h('div', { class: 'md-code-head' }, span('md-code-lang', lang || 'code'), copyBtn),
    h('div', { class: 'md-code-body' }, h('pre', { class: 'md-code-pre' }, codeText)));
  if (collapsible) {
    const toggleBtn = h('button', {
      type: 'button',
      class: 'md-code-expand',
      onclick: () => {
        const collapsed = wrap.classList.toggle('is-collapsed');
        toggleBtn.textContent = collapsed ? L(`展开全部代码（共 ${lineCount} 行）`, `Show all code (${lineCount} lines)`) : L('收起代码', 'Collapse code');
      },
    }, L(`展开全部代码（共 ${lineCount} 行）`, `Show all code (${lineCount} lines)`));
    wrap.appendChild(toggleBtn);
  }
  return wrap;
}

export function renderMarkdown(src) {
  const root = h('div', { class: 'md-body' });
  const lines = String(src || '').replace(/\r\n?/g, '\n').split('\n');
  let i = 0;
  let para = [];
  const flushPara = () => {
    if (!para.length) return;
    const kids = [];
    para.forEach((ln, idx) => {
      if (idx > 0) kids.push(h('br'));
      kids.push(...renderInlineMd(ln));
    });
    root.appendChild(h('p', null, kids));
    para = [];
  };
  while (i < lines.length) {
    const line = lines[i];
    const fence = line.match(/^\s*(`{3,})([^`]*)$/);
    if (fence) {
      flushPara();
      const fenceLen = fence[1].length;
      const lang = (fence[2] || '').trim();
      const buf = [];
      i++;
      while (i < lines.length && !isClosingFence(lines[i], fenceLen)) {
        buf.push(lines[i]);
        i++;
      }
      if (i < lines.length) i++;
      root.appendChild(renderCodeBlock(lang, buf.join('\n')));
      continue;
    }
    if (!line.trim()) {
      flushPara();
      i++;
      continue;
    }
    if (/^\s*(?:---+|\*\*\*+|___+|<hr\s*\/?>)\s*$/i.test(line)) {
      flushPara();
      root.appendChild(h('hr', { class: 'md-hr' }));
      i++;
      continue;
    }
    const hd = line.match(/^\s*(#{1,4})\s+(.*)$/);
    if (hd) {
      flushPara();
      const lvl = hd[1].length;
      root.appendChild(h(`h${Math.min(lvl + 2, 6)}`, { class: `md-h${lvl}` }, renderInlineMd(hd[2].trim())));
      i++;
      continue;
    }
    if (line.includes('|') && i + 1 < lines.length && isTableSep(lines[i + 1])) {
      flushPara();
      const headers = splitTableRow(line);
      const aligns = tableAligns(lines[i + 1]);
      i += 2;
      const bodyRows = [];
      while (i < lines.length && lines[i].includes('|') && lines[i].trim()) {
        bodyRows.push(splitTableRow(lines[i]));
        i++;
      }
      const tbl = h('div', { class: 'md-table-wrap' }, h('table', { class: 'md-table' },
        h('thead', null, h('tr', null, headers.map((cell, ci) => h('th', { style: aligns[ci] !== 'left' ? `text-align:${aligns[ci]}` : null }, renderInlineMd(cell))))),
        h('tbody', null, bodyRows.map((row) => h('tr', null, headers.map((_, ci) => h('td', { style: aligns[ci] !== 'left' ? `text-align:${aligns[ci]}` : null }, renderInlineMd(row[ci] ?? ''))))))));
      root.appendChild(tbl);
      continue;
    }
    if (/^\s*>\s?/.test(line)) {
      flushPara();
      const qLines = [];
      while (i < lines.length && /^\s*>\s?/.test(lines[i])) {
        qLines.push(lines[i].replace(/^\s*>\s?/, ''));
        i++;
      }
      root.appendChild(h('blockquote', { class: 'md-quote' }, renderMarkdown(qLines.join('\n'))));
      continue;
    }
    const listMatch = line.match(/^(\s*)(?:([-*+])|(\d+)\.)\s+(.*)$/);
    if (listMatch) {
      flushPara();
      const baseIndent = listMatch[1].length;
      const ordered = !!listMatch[3];
      const topList = h(ordered ? 'ol' : 'ul', { class: 'md-list' });
      let lastLi = null;
      let subList = null;
      let subOrdered = null;
      while (i < lines.length) {
        const m = lines[i].match(/^(\s*)(?:([-*+])|(\d+)\.)\s+(.*)$/);
        if (!m) break;
        const ind = m[1].length;
        const isOrd = !!m[3];
        if (ind >= baseIndent + 2 && lastLi) {
          if (!subList || subOrdered !== isOrd) {
            subList = h(isOrd ? 'ol' : 'ul', { class: 'md-list' });
            subOrdered = isOrd;
            lastLi.appendChild(subList);
          }
          subList.appendChild(h('li', null, renderInlineMd(m[4])));
        } else if (isOrd === ordered) {
          subList = null;
          lastLi = h('li', null, renderInlineMd(m[4]));
          topList.appendChild(lastLi);
        } else {
          break;
        }
        i++;
      }
      root.appendChild(topList);
      continue;
    }
    para.push(line.trim());
    i++;
  }
  flushPara();
  return root;
}

export const mdProse = renderMarkdown;
