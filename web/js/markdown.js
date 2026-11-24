// starfane web console — zero-dependency safe Markdown renderer
import { CODE_COLLAPSE_LINES } from './constants.js';
import { copyText, h, span } from './dom.js';

export function isSafeUrl(url) {
  const u = String(url || '').trim();
  return /^(https?:\/\/|file:\/\/)/i.test(u);
}

const INLINE_RE = /`([^`\n]+)`|\[((?:[^\]\n`]|`[^`\n]+`)+)\]\(([^)\s]+)\)|\*\*((?:[^*\n`]|\*(?!\*)|`[^`\n]+`)+?)\*\*|__([^_\n`]+?)__|~~((?:[^~\n`]|~(?!~)|`[^`\n]+`)+?)~~|\*((?:[^*\n`]|`[^`\n]+`)+?)\*/g;

export function renderInlineMd(text, depth = 0) {
  const out = [];
  const s = String(text || '');
  if (!s || depth > 4) return s ? [s] : out;
  const re = new RegExp(INLINE_RE.source, 'g');
  let last = 0;
  let m;
  while ((m = re.exec(s)) !== null) {
    if (m.index > last) out.push(s.slice(last, m.index));
    if (m[1] !== undefined) {
      out.push(h('code', { class: 'md-inline-code' }, m[1]));
    } else if (m[2] !== undefined) {
      const inner = renderInlineMd(m[2], depth + 1);
      if (isSafeUrl(m[3])) {
        const isHttp = /^https?:\/\//i.test(m[3]);
        out.push(h('a', { href: m[3], ...(isHttp ? { target: '_blank', rel: 'noopener noreferrer' } : {}) }, inner));
      } else {
        out.push(...inner);
      }
    } else if (m[4] !== undefined || m[5] !== undefined) {
      out.push(h('strong', null, renderInlineMd(m[4] ?? m[5], depth + 1)));
    } else if (m[6] !== undefined) {
      out.push(h('del', null, renderInlineMd(m[6], depth + 1)));
    } else if (m[7] !== undefined) {
      out.push(h('em', null, renderInlineMd(m[7], depth + 1)));
    }
    last = re.lastIndex;
  }
  if (last < s.length) out.push(s.slice(last));
  return out;
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
  const copyBtn = h('button', { type: 'button', class: 'md-copy-btn', onclick: (e) => copyText(codeText, e.currentTarget) }, '复制代码');
  const wrap = h('div', { class: `md-code-wrap${collapsible ? ' is-collapsed' : ''}` },
    h('div', { class: 'md-code-head' }, span('md-code-lang', lang || 'code'), copyBtn),
    h('div', { class: 'md-code-body' }, h('pre', { class: 'md-code-pre' }, codeText)));
  if (collapsible) {
    const toggleBtn = h('button', {
      type: 'button',
      class: 'md-code-expand',
      onclick: () => {
        const collapsed = wrap.classList.toggle('is-collapsed');
        toggleBtn.textContent = collapsed ? `展开全部代码（共 ${lineCount} 行）` : '收起代码';
      },
    }, `展开全部代码（共 ${lineCount} 行）`);
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
      const closeRe = new RegExp(`^\\s*\`{${fenceLen},}\\s*$`);
      const buf = [];
      i++;
      while (i < lines.length && !closeRe.test(lines[i])) {
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
    if (/^\s*(?:---+|\*\*\*+|___+)\s*$/.test(line)) {
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
