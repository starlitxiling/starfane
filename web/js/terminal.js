// starfane web console — ANSI stripping, Nerd Font detection + PUA compatibility fallback,
// chunked virtualized terminal rendering, and terminal-to-Markdown turn parsing.
import { GLYPH_MODE_KEY, TERM_CHUNK_LINES } from './constants.js';
import { h } from './dom.js';
import { APP, S, UI } from './store.js';

let cachedNerdSupport = null;
let userFontProbed = false;
let userFontLoaded = false;
let fontsReadyBound = false;

function formatFontFamilyToken(raw) {
  const clean = String(raw || '').trim();
  if (!clean) return '';
  return clean.includes(',') || clean.startsWith('"') || clean.startsWith("'") ? clean : `"${clean}"`;
}

function syncTermMonoCss() {
  const parts = [];
  const configured = formatFontFamilyToken(S.terminalFont);
  if (configured) parts.push(configured);
  if (userFontLoaded) parts.push('"StarfaneUserTermFont"');
  parts.push('var(--term-mono-base)');
  document.documentElement.style.setProperty('--term-mono', parts.join(', '));
}

function bindFontsReadyReprobe() {
  if (fontsReadyBound || typeof document === 'undefined' || !document.fonts) return;
  fontsReadyBound = true;
  const reprobe = () => {
    const prev = cachedNerdSupport;
    cachedNerdSupport = null;
    if (prev !== null && detectNerdFontSupport(true) !== prev) {
      APP.schedule();
    }
  };
  document.fonts.ready.then(reprobe).catch(() => {});
  if (typeof document.fonts.addEventListener === 'function') {
    document.fonts.addEventListener('loadingdone', reprobe);
  }
}

export function stripAnsi(raw) {
  return String(raw || '')
    .replace(/\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)/g, '')
    .replace(/\x1b\[[0-9;?]*[ -/]*[@-~]/g, '')
    .replace(/\x1b[@-Z\\-_]/g, '')
    .replace(/\r\n?/g, '\n')
    .replace(/\u00a0/g, ' ');
}

/**
 * Load optional user custom font (`~/.config/starfane/fonts/terminal.woff2`, only when reported
 * present by daemon state so DevTools sees zero 404s) and/or configured `web.terminal_font` family,
 * then re-probe Nerd Font glyph coverage.
 */
export function applyTerminalFontConfig(fontFamily, hasUserFont = false) {
  const clean = String(fontFamily || '').trim();
  if (clean && S.terminalFont !== clean) {
    S.terminalFont = clean;
    syncTermMonoCss();
    cachedNerdSupport = null;
  }
  if (hasUserFont && !userFontProbed && typeof FontFace !== 'undefined') {
    userFontProbed = true;
    const face = new FontFace('StarfaneUserTermFont', 'url(/assets/user-term-font.woff2)');
    face.load()
      .then((loaded) => {
        document.fonts.add(loaded);
        userFontLoaded = true;
        syncTermMonoCss();
        cachedNerdSupport = null;
        APP.schedule();
      })
      .catch(() => { /* optional user font failed to decode */ });
  }
}

function resolveComputedTermFont() {
  if (!document.body) return 'monospace';
  const probe = document.createElement('span');
  probe.style.cssText = 'position:fixed;left:-9999px;visibility:hidden;pointer-events:none;font-family:var(--term-mono)';
  document.body.appendChild(probe);
  const stack = getComputedStyle(probe).fontFamily || 'monospace';
  probe.remove();
  return stack;
}

/**
 * Offscreen <canvas> differential pixel probe comparing Nerd Font PUA glyphs rendered under the
 * resolved `--term-mono` stack against BOTH unassigned `.notdef` tofu probes (`\uEFFF`, `\uF8FE`)
 * AND plain system monospace (`ui-monospace, Menlo, Consolas, monospace`).
 * Eliminates macOS LastResort / fallback false positives.
 */
export function detectNerdFontSupport(force = false) {
  bindFontsReadyReprobe();
  if (cachedNerdSupport !== null && !force) return cachedNerdSupport;
  try {
    const canvas = document.createElement('canvas');
    canvas.width = 40;
    canvas.height = 40;
    const ctx = canvas.getContext('2d', { willReadFrequently: true });
    if (!ctx) {
      cachedNerdSupport = false;
      return false;
    }
    const fontStack = resolveComputedTermFont();
    const baselineStack = 'ui-monospace, "SF Mono", Menlo, Consolas, monospace';
    const renderBytes = (ch, stack) => {
      ctx.clearRect(0, 0, 40, 40);
      ctx.fillStyle = '#000';
      ctx.textBaseline = 'top';
      ctx.font = `24px ${stack}`;
      ctx.fillText(ch, 4, 4);
      const data = ctx.getImageData(0, 0, 40, 40).data;
      let nonZero = 0;
      const sig = [];
      for (let i = 3; i < data.length; i += 16) {
        const a = data[i];
        if (a > 20) nonZero++;
        sig.push(a > 60 ? 1 : 0);
      }
      return { nonZero, key: sig.join('') };
    };
    const notdefE = renderBytes('\uEFFF', fontStack);
    const notdefF = renderBytes('\uF8FE', fontStack);
    const probes = ['\uE0B0', '\uF179', '\uF07C', '\uF017'];
    let matched = 0;
    for (const g of probes) {
      const r = renderBytes(g, fontStack);
      const base = renderBytes(g, baselineStack);
      if (
        r.nonZero > 4
        && r.key !== notdefE.key
        && r.key !== notdefF.key
        && r.key !== base.key
      ) {
        matched++;
      }
    }
    cachedNerdSupport = matched >= 3;
    return cachedNerdSupport;
  } catch {
    cachedNerdSupport = false;
    return false;
  }
}

export function effectiveGlyphMode() {
  if (UI.termGlyphMode === 'raw') return 'raw';
  if (UI.termGlyphMode === 'compat') return 'compat';
  return detectNerdFontSupport() ? 'raw' : 'compat';
}

export function cycleGlyphMode() {
  const eff = effectiveGlyphMode();
  const next = eff === 'compat' ? 'raw' : 'compat';
  UI.termGlyphMode = next;
  try {
    localStorage.setItem(GLYPH_MODE_KEY, next);
  } catch { /* ignore */ }
  APP.rerender();
}

// Comprehensive Powerlevel10k / Starship / Oh-My-Zsh / Powerline / Nerd Font PUA -> Unicode map.
const PUA_GLYPH_MAP = new Map([
  // Powerline separators (strip/space so prompt pills read cleanly without tofu boxes)
  [0xE0B0, ' '], [0xE0B1, '›'], [0xE0B2, ' '], [0xE0B3, '‹'],
  [0xE0B4, ''], [0xE0B5, ''], [0xE0B6, ''], [0xE0B7, ''],
  [0xE0B8, ' '], [0xE0B9, '/'], [0xE0BA, ' '], [0xE0BB, '/'],
  [0xE0BC, ' '], [0xE0BD, '/'], [0xE0BE, ' '], [0xE0BF, '/'],
  [0xE0C0, ' '], [0xE0C2, ' '], [0xE0C4, ' '], [0xE0C6, ' '],
  // Git & VCS (Powerlevel10k / Starship / Devicons / Octicons / Material Design)
  [0xE0A0, '⎇'], [0xE0A1, 'L'], [0xE0A2, '🔒'], [0xE0A3, 'C'],
  [0xE725, '⎇'], [0xF126, '⎇'], [0xF418, '⎇'], [0xF062C, '⎇'],
  [0xE702, 'git'], [0xE709, 'git'], [0xF09B, 'git'], [0xF113, 'git'],
  [0xF408, 'git'], [0xF1D3, 'git'], [0xF417, 'git'], [0xF02A2, 'git'],
  [0xF296, 'gitlab'], [0xF171, 'bb'],
  [0xE727, '⑂'], [0xE729, '@'], [0xF02B, '#'], [0xF01C, '⚑'],
  [0xF407, '●'], [0xF444, '●'],
  // OS / Host icons (macOS Apple, Linux, Ubuntu, Arch, Debian, Windows, Terminal)
  [0xF179, '⌘'], [0xF302, '⌘'], [0xE711, '⌘'],
  [0xF17C, '🐧'], [0xF31A, '🐧'], [0xF31B, '🐧'], [0xF303, '🐧'], [0xF306, '🐧'], [0xF30A, '🐧'],
  [0xF17A, '⊞'], [0xE70F, '⊞'],
  [0xF120, '›'], [0xF489, '›'], [0xEA85, '›'],
  // Folders & Home & Files
  [0xF07B, '📁'], [0xF07C, '📂'], [0xF114, '📁'], [0xF115, '📂'], [0xF413, '📁'], [0xEA83, '📁'],
  [0xF024B, '📁'], [0xF0256, '📂'],
  [0xF015, '⌂'], [0xF46D, '⌂'], [0xF02DC, '⌂'],
  [0xF15B, '📄'], [0xF4A5, '📄'],
  // Clock / Time / Duration
  [0xF017, '◷'], [0xF64F, '◷'], [0xF43A, '◷'], [0xE383, '◷'], [0xF0954, '◷'], [0xF0150, '◷'],
  [0xF251, '⏳'], [0xF252, '⏳'], [0xF254, '⏳'], [0xF0E4, '⏱'],
  // Check / Error / Warning / Info / Bolt / Lock / Key / User / Gear
  [0xF00C, '✓'], [0xF058, '✓'], [0xF42E, '✓'], [0xF012C, '✓'],
  [0xF00D, '✕'], [0xF057, '✕'], [0xF467, '✕'], [0xF0156, '✕'],
  [0xF071, '⚠'], [0xF06A, '⚠'], [0xF421, '⚠'], [0xEA6C, '⚠'], [0xF12A, '!'],
  [0xF0E7, '⚡'], [0xF49B, '⚡'],
  [0xF023, '🔒'], [0xF456, '🔒'], [0xF084, '🔑'],
  [0xF007, '👤'], [0xF415, '👤'],
  [0xF013, '⚙'], [0xF085, '⚙'], [0xF2DB, '⚙'], [0xF002, '🔍'],
  [0xF0EB, '💡'], [0xF0A0, '💾'], [0xF135, '🚀'], [0xF011, '⏻'], [0xF240, '🔋'],
  // Runtimes / Languages / Tools
  [0xE73C, 'py'], [0xE235, 'py'], [0xE606, 'py'], [0xF0320, 'py'],
  [0xE7A8, 'rs'], [0xF1617, 'rs'],
  [0xE718, 'node'], [0xED0D, 'node'], [0xE781, 'js'],
  [0xE627, 'go'], [0xE626, 'go'],
  [0xF308, 'docker'], [0xE7B0, 'docker'],
  [0xF487, 'pkg'], [0xE71E, 'npm'], [0xF1B3, 'box'],
  [0xE77F, 'c#'], [0xE61E, 'c'], [0xE61D, 'c++'], [0xE738, 'java'],
  [0xE791, 'rb'], [0xF0C2, '☁'], [0xE7AA, '☁'],
  // Arrows / Prompts / Bullets
  [0xF061, '→'], [0xF054, '›'], [0xF105, '›'], [0xF101, '»'],
  [0xF0DA, '▸'], [0xF0D7, '▾'], [0xF111, '●'],
]);

/**
 * Replace Nerd Font / Powerline Private Use Area code points with crisp universal Unicode
 * equivalents so prompts never render as ugly tofu boxes when Nerd Fonts are unavailable.
 */
export function normalizeTerminalGlyphs(text) {
  if (!text) return '';
  let out = '';
  for (const ch of String(text)) {
    const cp = ch.codePointAt(0);
    const mapped = PUA_GLYPH_MAP.get(cp);
    if (mapped !== undefined) {
      out += mapped;
    } else if (
      (cp >= 0xE000 && cp <= 0xF8FF)
      || (cp >= 0xF0000 && cp <= 0xFFFFD)
      || (cp >= 0x100000 && cp <= 0x10FFFD)
    ) {
      // Unmapped Nerd Font PUA glyph -> drop cleanly rather than displaying a missing-glyph tofu box.
      if (out.length && !/\s$/.test(out)) out += ' ';
    } else {
      out += ch;
    }
  }
  return out;
}

export function formatTerminalText(raw) {
  const clean = stripAnsi(raw);
  return effectiveGlyphMode() === 'compat' ? normalizeTerminalGlyphs(clean) : clean;
}

/**
 * Render terminal text into 200-line `<pre class="term-pre term-chunk">` blocks styled with
 * `content-visibility: auto`, allowing 50,000+ scrollback lines to render smoothly without DOM jank.
 */
export function renderChunkedTerminal(text) {
  const lines = String(text || '').split('\n');
  if (lines.length <= TERM_CHUNK_LINES + 50) {
    return h('pre', { class: 'term-pre' }, text);
  }
  const chunks = [];
  for (let i = 0; i < lines.length; i += TERM_CHUNK_LINES) {
    const slice = lines.slice(i, i + TERM_CHUNK_LINES).join('\n');
    chunks.push(h('pre', { class: 'term-pre term-chunk' }, slice));
  }
  return h('div', { class: 'term-chunks' }, chunks);
}

const PURE_BOX_RE = /^\s*[─━═╭╮╰╯┌┐└┘├┤┬┴┼│\s]{4,}\s*$/;
const TUI_CHROME_RE = /(?:^\s*\?\s*for shortcuts\b|shift\+tab to cycle|esc to interrupt|alt\+s\b|context left\b|←\s*(?:\d+\s+)?agents?\b|←\s*for agents\b|^\s*›\s*Ask Codex to do anything\s*$|^\s*⏵⏵\s*auto mode\b|^\s*●\s*\[\d{2}:\d{2}:\d{2}\]\s*cron\b|^\s*(?:Opus|Sonnet|Haiku|Claude|GPT-[^\s|]+)\s+[^|\n]*(?:\|\s*~\/|·\s*~\/))/i;
const EMPTY_PROMPT_RE = /^\s*[❯›>]\s*$/;
const WORKED_FOR_RE = /^\s*(?:Worked for\s+\S+\s*[•·]|[✻✳✶✢·*]\s+[A-Z][a-z]+\s+for\s+\d)/i;

export function cleanTerminalToMarkdown(raw, _agent) {
  const cleaned = formatTerminalText(raw);
  const lines = cleaned.split('\n').map((l) => l.replace(/\s+$/, ''));
  while (lines.length > 0) {
    const t = lines[lines.length - 1];
    if (!t.trim() || PURE_BOX_RE.test(t) || EMPTY_PROMPT_RE.test(t) || TUI_CHROME_RE.test(t) || WORKED_FOR_RE.test(t)) {
      lines.pop();
    } else break;
  }
  const out = [];
  let inFence = false;
  for (const orig of lines) {
    let l = orig;
    const boxWrap = l.match(/^\s*│\s?(.*?)\s?│\s*$/);
    if (boxWrap) l = boxWrap[1];
    if (/^\s*```/.test(l)) {
      inFence = !inFence;
      out.push(l.replace(/^ {1,2}(?=```)/, ''));
      continue;
    }
    if (inFence) {
      out.push(l);
      continue;
    }
    if (TUI_CHROME_RE.test(l) || EMPTY_PROMPT_RE.test(l) || WORKED_FOR_RE.test(l)) continue;
    if (PURE_BOX_RE.test(l)) {
      const ruleChars = l.replace(/\s+/g, '');
      if (ruleChars.length >= 4 && ruleChars.length <= 18 && /^[─━═]+$/.test(ruleChars)) out.push('---');
      continue;
    }
    if (l.startsWith('  ') && !/^ {4}/.test(l)) l = l.slice(2);
    const subBullet = l.match(/^(\s{2,})[•●▪]\s+(.*)$/);
    if (subBullet) { out.push(`  - ${subBullet[2]}`); continue; }
    const topBullet = l.match(/^\s*[•●▪]\s+(.*)$/);
    if (topBullet) { out.push(`- ${topBullet[1]}`); continue; }
    const rec = l.match(/^\s*⏺\s+(.*)$/);
    if (rec) { out.push(rec[1]); continue; }
    const sub = l.match(/^\s*⎿\s+(.*)$/);
    if (sub) { out.push(`> ${sub[1]}`); continue; }
    const hd = l.match(/^\s{1,4}(#{1,4}\s+.*)$/);
    if (hd) { out.push(hd[1]); continue; }
    const usr = l.match(/^\s*[❯›]\s+(.+)$/);
    if (usr && !/^Ask Codex to do anything$/i.test(usr[1].trim())) {
      out.push('', `> **用户输入**：${usr[1].trim()}`, '');
      continue;
    }
    out.push(l);
  }
  return out.join('\n').replace(/\n{3,}/g, '\n\n').trim();
}

export function parseTerminalTurns(raw, agent) {
  const cleaned = formatTerminalText(raw);
  const lines = cleaned.split('\n').map((l) => l.replace(/\s+$/, ''));
  while (lines.length > 0) {
    const t = lines[lines.length - 1];
    if (!t.trim() || PURE_BOX_RE.test(t) || EMPTY_PROMPT_RE.test(t) || TUI_CHROME_RE.test(t) || WORKED_FOR_RE.test(t)) lines.pop();
    else break;
  }
  const turns = [];
  let buf = [];
  const flushAsst = () => {
    const md = cleanTerminalToMarkdown(buf.join('\n'), agent);
    if (md) turns.push({ role: 'assistant', text: md });
    buf = [];
  };
  for (const l of lines) {
    const pm = l.match(/^\s*[❯›]\s+(.+)$/);
    if (pm && !/^Ask Codex to do anything$/i.test(pm[1].trim())) {
      flushAsst();
      turns.push({ role: 'user', text: pm[1].trim() });
    } else {
      buf.push(l);
    }
  }
  flushAsst();
  return turns;
}
