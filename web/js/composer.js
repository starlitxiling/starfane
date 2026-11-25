// starfane web console — direct composer, paste/image attachments, slash menu, and pipeline composer
import {
  IMG_MAX_BYTES,
  PARSE_DEBOUNCE_MS,
  PASTE_CHAR_LIMIT,
  PASTE_LINE_LIMIT,
  PLAYBOOK_STAGES,
} from './constants.js';
import {
  agentTag,
  btn,
  errText,
  field,
  fmtBytes,
  h,
  shorthand,
  span,
  toast,
  warnText,
} from './dom.js';
import { act, api, loadPreview, loadSlashCommands } from './api.js';
import { L } from './i18n.js';
import {
  APP,
  S,
  UI,
  currentSession,
  draft,
  enc,
  hostOf,
  knownProjects,
  nextAttachId,
  pvModeOf,
  slashCommandsFor,
  stageName,
  upsertPipeline,
} from './store.js';

export function autoGrow(ta) {
  if (!ta) return;
  const minH = 52;
  const maxH = 220;
  if (ta.offsetHeight >= maxH - 1 && ta.scrollHeight > maxH + 2) {
    ta.style.height = `${maxH}px`;
    return;
  }
  const prevTop = ta.scrollTop;
  ta.style.height = 'auto';
  const nextH = Math.min(maxH, Math.max(minH, ta.scrollHeight));
  ta.style.height = `${nextH}px`;
  if (prevTop > 0) ta.scrollTop = prevTop;
}

function makeAttachToken(kind, num) {
  return kind === 'image' ? L(`[图片 #${num}]`, `[Image #${num}]`) : L(`[粘贴 #${num}]`, `[Paste #${num}]`);
}

function currentComposerText(sid) {
  const ta = UI.wsShell?.comp?.sid === sid ? UI.wsShell.comp.ta : null;
  return ta ? ta.value : draft(`ws:msg:${sid}`);
}

function nextAttachmentNum(list, kind, curText) {
  let max = 0;
  for (const item of list) {
    if (item.kind === kind && Number.isFinite(item.num)) max = Math.max(max, item.num);
  }
  let n = max + 1;
  while (curText.includes(makeAttachToken(kind, n))) n += 1;
  return n;
}

function clampRange(range, len) {
  const a = Math.max(0, Math.min(len, Number(range?.[0] ?? len)));
  const b = Math.max(a, Math.min(len, Number(range?.[1] ?? a)));
  return [a, b];
}

function resolveInsertRange(sid, cur, ta) {
  const len = cur.length;
  if (ta && document.activeElement === ta && ta.selectionStart != null) {
    return clampRange([ta.selectionStart, ta.selectionEnd ?? ta.selectionStart], len);
  }
  const comp = UI.wsShell?.comp;
  if (comp?.lastSel && comp.lastSel.sid === sid) {
    return clampRange([comp.lastSel.start, comp.lastSel.end], len);
  }
  if (ta && ta.selectionStart != null) {
    return clampRange([ta.selectionStart, ta.selectionEnd ?? ta.selectionStart], len);
  }
  return [len, len];
}

export function saveComposerSelection(ta) {
  const comp = UI.wsShell?.comp;
  if (!comp || comp.ta !== ta || !comp.sid) return;
  const len = ta.value.length;
  comp.lastSel = {
    sid: comp.sid,
    start: ta.selectionStart ?? len,
    end: ta.selectionEnd ?? len,
  };
}

function insertTextAtCursor(sid, snippet, opts = {}) {
  if (!sid) return null;
  const dk = `ws:msg:${sid}`;
  const comp = UI.wsShell?.comp;
  const ta = comp?.sid === sid ? comp.ta : null;
  const cur = ta ? ta.value : draft(dk);
  const [start, end] = opts.range ? clampRange(opts.range, cur.length) : resolveInsertRange(sid, cur, ta);
  const before = cur.slice(0, start);
  const after = cur.slice(end);
  const lead = opts.inlineToken && before.length > 0 && !/\s$/.test(before) ? ' ' : '';
  const extraAfter = opts.trailingNewline && !after.startsWith('\n') ? '\n' : '';
  const trail = !extraAfter && opts.inlineToken && after.length > 0 && !/^\s/.test(after) ? ' ' : '';
  const next = before + lead + snippet + extraAfter + trail + after;
  const caret = before.length + lead.length + snippet.length + extraAfter.length;
  UI.drafts.set(dk, next);
  if (comp?.sid === sid) comp.lastSel = { sid, start: caret, end: caret };
  if (ta) {
    ta.value = next;
    ta.style.height = 'auto';
    autoGrow(ta);
    if (opts.focus) ta.focus({ preventScroll: true });
    ta.setSelectionRange(caret, caret);
  }
  return { next, caret };
}

function removeTokenFromDraft(sid, token) {
  if (!sid || !token) return;
  const dk = `ws:msg:${sid}`;
  const comp = UI.wsShell?.comp;
  const ta = comp?.sid === sid ? comp.ta : null;
  const cur = ta ? ta.value : draft(dk);
  const idx = cur.indexOf(token);
  if (idx < 0) return;
  let before = cur.slice(0, idx);
  let after = cur.slice(idx + token.length);
  if (before.endsWith(' ') && after.startsWith(' ')) {
    after = after.slice(1);
  } else if ((before === '' || before.endsWith('\n')) && after.startsWith('\n')) {
    after = after.slice(1);
  } else if (before.endsWith('\n') && after === '') {
    before = before.slice(0, -1);
  } else if (before.endsWith(' ') && (after === '' || after.startsWith('\n'))) {
    before = before.slice(0, -1);
  } else if (before === '' && after.startsWith(' ')) {
    after = after.slice(1);
  }
  const next = before + after;
  UI.drafts.set(dk, next);
  if (comp?.sid === sid) {
    const caret = Math.min(before.length, next.length);
    comp.lastSel = { sid, start: caret, end: caret };
  }
  if (ta) {
    ta.value = next;
    ta.style.height = 'auto';
    autoGrow(ta);
  }
}

function locateOrReinsertToken(sid, att) {
  if (!sid || !att?.token) return;
  const comp = UI.wsShell?.comp;
  const ta = comp?.sid === sid ? comp.ta : null;
  const cur = ta ? ta.value : draft(`ws:msg:${sid}`);
  const idx = cur.indexOf(att.token);
  if (idx >= 0) {
    if (ta) {
      ta.focus({ preventScroll: true });
      ta.setSelectionRange(idx, idx + att.token.length);
      saveComposerSelection(ta);
    }
    return;
  }
  insertTextAtCursor(sid, att.token, { inlineToken: true, focus: true });
  toast(L(`已重新插入 ${att.token}`, `Re-inserted ${att.token}`), 'ok');
}

export function attachFoldedPaste(sid, rawText, opts = {}) {
  if (!sid) return null;
  const normalized = String(rawText || '').replace(/\r\n?/g, '\n');
  if (!normalized) return null;
  const list = UI.attachments.get(sid) || [];
  const num = nextAttachmentNum(list, 'text', currentComposerText(sid));
  const token = makeAttachToken('text', num);
  const lines = normalized.split('\n').length;
  const bytes = new TextEncoder().encode(normalized).length;
  const item = {
    id: nextAttachId(),
    num,
    token,
    kind: 'text',
    title: L(`粘贴片段 #${num}`, `Paste snippet #${num}`),
    content: normalized,
    lines,
    bytes,
  };
  list.push(item);
  UI.attachments.set(sid, list);
  insertTextAtCursor(sid, token, {
    inlineToken: true,
    range: opts.range,
    trailingNewline: opts.trailingNewline,
    focus: opts.focus,
  });
  renderAttachBar(sid);
  return item;
}

export function quoteReplyToComposer(sid, rawText) {
  if (!sid) return;
  const text = String(rawText || '').replace(/\r\n?/g, '\n').trim();
  if (!text) return;
  const comp = UI.wsShell?.comp;
  if (UI.composerMode !== 'direct') {
    UI.composerMode = 'direct';
    if (comp) {
      comp.directRow?.classList.remove('ws-hidden');
      comp.pipeSlot?.classList.add('ws-hidden');
    }
    APP.rerender();
  }
  const lines = text.split('\n');
  if (text.length < PASTE_CHAR_LIMIT && lines.length < PASTE_LINE_LIMIT) {
    const cur = currentComposerText(sid);
    const ta = comp?.sid === sid ? comp.ta : null;
    const [start, end] = resolveInsertRange(sid, cur, ta);
    const before = cur.slice(0, start);
    const after = cur.slice(end);
    const lead = before.length === 0 || before.endsWith('\n') ? '' : '\n';
    const quoted = lines.map((l) => (l.length ? `> ${l}` : '>')).join('\n');
    const trail = after.length === 0 ? '\n\n' : (after.startsWith('\n\n') ? '' : (after.startsWith('\n') ? '\n' : '\n\n'));
    insertTextAtCursor(sid, `${lead}${quoted}${trail}`, { range: [start, end], focus: true });
    toast(L('已引用到输入框，可继续在其下方提问', 'Quoted into composer — type your follow-up below'), 'ok');
    return;
  }
  const item = attachFoldedPaste(sid, text, { trailingNewline: true, focus: true });
  if (item) toast(L(`长回复已折叠为 ${item.token}，可直接在其下方继续提问`, `Long reply folded into ${item.token} — type your follow-up below`), 'ok');
}

export function removeAttachment(sid, attId, opts = {}) {
  const prev = UI.attachments.get(sid) || [];
  const removed = prev.find((x) => x.id === attId);
  const list = prev.filter((x) => x.id !== attId);
  if (list.length) UI.attachments.set(sid, list);
  else UI.attachments.delete(sid);
  if (!opts.keepToken && removed?.token) removeTokenFromDraft(sid, removed.token);
  renderAttachBar(sid);
}

export function expandAttachmentToInput(sid, attId) {
  const list = UI.attachments.get(sid) || [];
  const item = list.find((x) => x.id === attId);
  if (!item) return;
  const dk = `ws:msg:${sid}`;
  const comp = UI.wsShell?.comp;
  const ta = comp?.sid === sid ? comp.ta : null;
  const cur = ta ? ta.value : draft(dk);
  const idx = item.token ? cur.indexOf(item.token) : -1;
  let next;
  let caret;
  if (idx >= 0) {
    next = cur.slice(0, idx) + item.content + cur.slice(idx + item.token.length);
    caret = idx + item.content.length;
  } else {
    next = cur ? `${cur}\n${item.content}` : item.content;
    caret = next.length;
  }
  UI.drafts.set(dk, next);
  if (comp?.sid === sid) comp.lastSel = { sid, start: caret, end: caret };
  if (ta) {
    ta.value = next;
    ta.style.height = 'auto';
    autoGrow(ta);
    ta.focus({ preventScroll: true });
    ta.setSelectionRange(caret, caret);
  }
  removeAttachment(sid, attId, { keepToken: true });
}

function readFileAsBase64(file) {
  return new Promise((resolve, reject) => {
    const fr = new FileReader();
    fr.onload = () => {
      const res = String(fr.result || '');
      const idx = res.indexOf(',');
      resolve(idx >= 0 ? res.slice(idx + 1) : res);
    };
    fr.onerror = () => reject(new Error(L('读取图片文件失败', 'Failed to read image file')));
    fr.readAsDataURL(file);
  });
}

export async function uploadImageFiles(sid, files, opts = {}) {
  if (!sid) return;
  const imgs = [...(files || [])].filter((f) => f && String(f.type || '').startsWith('image/'));
  if (!imgs.length) return;
  const queued = [];
  for (const file of imgs) {
    if (file.size > IMG_MAX_BYTES) {
      toast(L(`图片 ${file.name || ''} 超过 10 MB 限制`, `Image ${file.name || ''} exceeds 10 MB limit`), 'warn');
      continue;
    }
    const ext = (file.type.split('/')[1] || 'png').replace(/[^a-z0-9]/gi, '') || 'png';
    const name = file.name && file.name !== 'image.png'
      ? file.name
      : `screenshot-${new Date().toTimeString().slice(0, 8).replace(/:/g, '')}.${ext}`;
    const list = UI.attachments.get(sid) || [];
    const num = nextAttachmentNum(list, 'image', currentComposerText(sid));
    const token = makeAttachToken('image', num);
    const item = {
      id: nextAttachId(),
      num,
      token,
      kind: 'image',
      title: name,
      filename: name,
      mime: file.type || 'image/png',
      bytes: file.size || 0,
      status: 'uploading',
      previewUrl: '',
      path: '',
    };
    list.push(item);
    UI.attachments.set(sid, list);
    const range = queued.length === 0 ? opts.range : undefined;
    insertTextAtCursor(sid, token, { inlineToken: true, range, focus: opts.focus });
    queued.push({ file, item, name });
  }
  if (!queued.length) return;
  renderAttachBar(sid);
  const targetSess = S.sessions.get(sid) || currentSession();
  const targetHostId = hostOf(targetSess);
  for (const { file, item, name } of queued) {
    try {
      const data_base64 = await readFileAsBase64(file);
      const body = { filename: name, mime: item.mime, data_base64 };
      if (targetHostId !== 'local') body.host_id = targetHostId;
      const r = await api('POST', '/api/uploads/image', body);
      item.status = 'ready';
      item.path = r.path || '';
      item.previewUrl = r.preview_url || '';
      item.bytes = r.size_bytes ?? item.bytes;
      item.filename = r.filename || item.filename;
      item.title = item.filename;
      renderAttachBar(sid);
      toast(L(`图片已添加为 ${item.token}（${fmtBytes(item.bytes)}）`, `Image attached as ${item.token} (${fmtBytes(item.bytes)})`), 'ok');
    } catch (e) {
      removeAttachment(sid, item.id);
      toast(L(`图片上传失败：${errText(e)}`, `Image upload failed: ${errText(e)}`), 'err');
    }
  }
}

export function renderAttachBar(sid) {
  if (!UI.wsShell?.comp) return;
  const bar = UI.wsShell.comp.attachBar;
  const list = sid ? (UI.attachments.get(sid) || []) : [];
  const sig = list.map((x) => `${x.id}:${x.token || ''}:${x.status || 'ok'}:${x.previewUrl || ''}`).join(',');
  if (bar.dataset.sid === (sid || '') && bar.dataset.sig === sig && bar.classList.contains('ws-hidden') === (list.length === 0)) return;
  bar.dataset.sid = sid || '';
  bar.dataset.sig = sig;
  if (!list.length || UI.composerMode !== 'direct') {
    bar.classList.add('ws-hidden');
    bar.replaceChildren();
    return;
  }
  bar.classList.remove('ws-hidden');
  bar.replaceChildren(...list.map((att) => {
    const tokBtn = att.token ? h('button', {
      type: 'button',
      class: 'attach-tok',
      title: L('点击在输入框定位该占位符，若已删掉则重新插入', 'Click to locate placeholder in composer (or re-insert if deleted)'),
      onclick: () => locateOrReinsertToken(sid, att),
    }, att.token) : null;
    if (att.kind === 'image') {
      const uploading = att.status === 'uploading';
      const thumb = uploading
        ? h('span', { class: 'attach-thumb-load', title: L('图片上传中…', 'Uploading image…') })
        : h('button', {
          type: 'button',
          class: 'attach-thumb-btn',
          title: L('点击放大预览原图', 'Click to preview full image'),
          onclick: () => APP.openDrawer('paste', { sessionId: sid, attachmentId: att.id }),
        }, h('img', { class: 'attach-thumb', src: att.previewUrl, alt: att.title }));
      return h('div', { class: 'attach-pill img-pill' },
        thumb,
        tokBtn,
        h('span', { class: 'attach-info' },
          span('attach-meta', att.title),
          span('attach-sub', uploading ? L('上传中…', 'Uploading…') : fmtBytes(att.bytes))),
        h('span', { class: 'attach-acts' },
          !uploading ? h('button', { type: 'button', class: 'attach-act', onclick: () => APP.openDrawer('paste', { sessionId: sid, attachmentId: att.id }) }, L('预览', 'Preview')) : null,
          h('button', { type: 'button', class: 'attach-act del', title: L('移除图片', 'Remove image'), 'aria-label': L('移除图片', 'Remove image'), onclick: () => removeAttachment(sid, att.id) }, '×')));
    }
    return h('div', { class: 'attach-pill' },
      tokBtn,
      span('attach-meta', L(`${att.lines} 行 · ${fmtBytes(att.bytes)}`, `${att.lines} lines · ${fmtBytes(att.bytes)}`)),
      h('span', { class: 'attach-acts' },
        h('button', { type: 'button', class: 'attach-act', onclick: () => APP.openDrawer('paste', { sessionId: sid, attachmentId: att.id }) }, L('预览', 'Preview')),
        h('button', { type: 'button', class: 'attach-act', onclick: () => expandAttachmentToInput(sid, att.id) }, L('转入输入框', 'Expand inline')),
        h('button', { type: 'button', class: 'attach-act del', title: L('移除粘贴片段', 'Remove snippet'), 'aria-label': L('移除粘贴片段', 'Remove snippet'), onclick: () => removeAttachment(sid, att.id) }, '×')));
  }));
}

export function closeSlashMenu() {
  UI.slash.open = false;
  UI.slash.items = [];
  UI.slash.idx = 0;
  if (UI.wsShell?.comp?.slashEl) {
    UI.wsShell.comp.slashEl.classList.add('ws-hidden');
    UI.wsShell.comp.slashEl.replaceChildren();
  }
}

export function syncSlashActive() {
  const box = UI.wsShell?.comp?.slashEl;
  if (!box) return;
  for (const el of box.querySelectorAll('.slash-item[data-sidx]')) {
    const on = Number(el.dataset.sidx) === UI.slash.idx;
    el.classList.toggle('active', on);
    if (on) el.scrollIntoView({ block: 'nearest' });
  }
}

export function renderSlashMenu(agent) {
  const box = UI.wsShell?.comp?.slashEl;
  if (!box) return;
  const { open, items, idx } = UI.slash;
  if (!open || !items.length || UI.composerMode !== 'direct') {
    box.classList.add('ws-hidden');
    box.replaceChildren();
    return;
  }
  const nativeItems = [];
  const starfaneItems = [];
  items.forEach((it, flatIdx) => {
    if (it.group === 'starfane') starfaneItems.push([it, flatIdx]);
    else nativeItems.push([it, flatIdx]);
  });
  const mkRow = ([it, flatIdx]) => h('button', {
    type: 'button',
    class: `slash-item${flatIdx === idx ? ' active' : ''}`,
    'data-sidx': String(flatIdx),
    onmousedown: (e) => e.preventDefault(),
    onmouseenter: () => { UI.slash.idx = flatIdx; syncSlashActive(); },
    onclick: () => applySlashItem(it),
  },
  h('span', { class: 'slash-left' },
    span('slash-cmd', it.cmd),
    it.group === 'skill' ? span('tag', 'skill') : null,
    it.args_hint ? span('slash-args', it.args_hint) : null),
  span('slash-desc', it.desc || ''));
  const listNodes = [];
  if (nativeItems.length) {
    listNodes.push(h('div', { class: 'slash-group' }, L('Agent 原生命令', 'Agent Native Commands'), agentTag(agent)));
    listNodes.push(...nativeItems.map(mkRow));
  }
  if (starfaneItems.length) {
    listNodes.push(h('div', { class: 'slash-group' }, L('Starfane 控制台快捷命令', 'Starfane Console Shortcuts')));
    listNodes.push(...starfaneItems.map(mkRow));
  }
  box.classList.remove('ws-hidden');
  box.replaceChildren(
    h('div', { class: 'slash-list', role: 'listbox', 'aria-label': L('斜杠命令菜单', 'Slash command menu') }, listNodes),
    h('div', { class: 'slash-foot' },
      h('span', null, L('↑↓ 选择 · Enter / Tab 确认 · Esc 关闭', '↑↓ select · Enter / Tab confirm · Esc close')),
      span('tabular', L(`${items.length} 条命令`, `${items.length} commands`))),
  );
}

export function updateSlashFromInput(ta, agent) {
  if (!ta || UI.composerMode !== 'direct') { closeSlashMenu(); return; }
  const pos = ta.selectionStart;
  if (pos == null || pos !== ta.selectionEnd) { closeSlashMenu(); return; }
  const before = ta.value.slice(0, pos);
  const m = before.match(/^\s*(\/[^\s/]*)$/);
  if (!m) { closeSlashMenu(); return; }
  const token = m[1];
  const start = before.indexOf(token);
  const q = token.slice(1).toLowerCase();
  const ag = agent || UI.wsShell?.comp?.agent || 'claude';
  loadSlashCommands(ag);
  const all = slashCommandsFor(ag);
  const filtered = q ? all.filter((c) => c.cmd.toLowerCase().includes(q) || (c.desc || '').toLowerCase().includes(q)) : all;
  if (!filtered.length) { closeSlashMenu(); return; }
  const prevCmd = UI.slash.open && UI.slash.items[UI.slash.idx]?.cmd;
  let nextIdx = prevCmd ? filtered.findIndex((c) => c.cmd === prevCmd) : 0;
  if (nextIdx < 0) nextIdx = 0;
  UI.slash = { open: true, start, end: pos, query: q, items: filtered, idx: nextIdx };
  renderSlashMenu(ag);
}

function runStarfaneSlashAction(action) {
  if (action === 'mode_feature') { UI.composerMode = 'feature'; APP.rerender(); }
  else if (action === 'mode_debug') { UI.composerMode = 'debug'; APP.rerender(); }
  else if (action === 'open_mute') { UI.muteMenu = true; APP.rerender(); }
  else if (action === 'toggle_terminal') {
    UI.outputMode = UI.outputMode === 'raw' ? 'reading' : 'raw';
    if (UI.outputMode === 'raw') {
      const cur = currentSession();
      if (cur?.pane) loadPreview(cur.pane, { mode: pvModeOf(cur.pane), silent: true });
    }
    APP.rerender();
  } else if (action === 'open_drawer') {
    const cur = currentSession();
    if (cur) APP.openDrawer('session', { sessionId: cur.session_id });
  }
}

export function applySlashItem(item) {
  const ta = UI.wsShell?.comp?.ta;
  if (!ta || !item) return;
  const sid = UI.wsShell.comp.sid;
  const { start, end } = UI.slash;
  const head = ta.value.slice(0, start);
  const tail = ta.value.slice(end);
  closeSlashMenu();
  if (item.group === 'starfane' || item.action) {
    const cleaned = (head + tail.replace(/^\s/, '')).trimEnd();
    ta.value = cleaned;
    if (sid) UI.drafts.set(`ws:msg:${sid}`, cleaned);
    ta.style.height = 'auto';
    autoGrow(ta);
    runStarfaneSlashAction(item.action);
    return;
  }
  const ins = item.needs_args ? `${item.cmd} ` : item.cmd;
  const nextVal = head + ins + tail;
  const caret = start + ins.length;
  ta.value = nextVal;
  if (sid) UI.drafts.set(`ws:msg:${sid}`, nextVal);
  autoGrow(ta);
  ta.focus({ preventScroll: true });
  ta.setSelectionRange(caret, caret);
}

const parseTimers = {};
const parseSeq = {};

export function queueParse(scope, playbook, projKey, textKey, defaultProj) {
  clearTimeout(parseTimers[scope]);
  const text = draft(textKey).trim();
  if (!text) { delete UI.parsePreviews[scope]; APP.schedule(); return; }
  parseTimers[scope] = setTimeout(async () => {
    const seq = (parseSeq[scope] = (parseSeq[scope] || 0) + 1);
    const project = draft(projKey, defaultProj || '').trim();
    try {
      const data = await api('POST', '/api/pipelines/parse', { text, playbook, ...(project ? { project } : {}) });
      if (seq === parseSeq[scope]) UI.parsePreviews[scope] = { text, data };
    } catch (e) {
      if (seq === parseSeq[scope]) UI.parsePreviews[scope] = { text, err: errText(e) };
    }
    APP.schedule();
  }, PARSE_DEBOUNCE_MS);
}

export function workspacePipelineComposer(s, mode) {
  const isDebug = mode === 'debug';
  const scope = `ws:${mode}`;
  const goalKey = `${scope}:goal`;
  const bindKey = `${scope}:bind`;
  const projKey = `${scope}:proj`;
  const defaultProj = s.git_root || s.cwd || knownProjects()[0] || '';
  const pv = UI.parsePreviews[scope];
  const bindInp = field(bindKey, {
    class: 'inp sm',
    autocomplete: 'off',
    placeholder: isDebug
      ? L('选填：如 快速排查用 agy，深挖根因用 codex', 'Optional: e.g. triage with agy, deep root-cause with codex')
      : L('选填：如 计划用 claude，实现用 codex，Review 用 agy', 'Optional: e.g. plan with claude, implement with codex, review with agy'),
  });
  bindInp.addEventListener('input', () => queueParse(scope, mode, projKey, bindKey, defaultProj));
  const projInp = field(projKey, {
    class: 'inp sm mono',
    autocomplete: 'off',
    placeholder: L('选填项目目录（留空使用当前会话目录）', 'Optional project directory (defaults to session cwd)'),
  }, defaultProj);
  const ta = field(goalKey, {
    tag: 'textarea',
    rows: 2,
    class: 'composer-ta',
    placeholder: isDebug
      ? L('描述故障现象或报错（Cmd+Enter 启动两速查错）…', 'Describe symptom or error (Cmd+Enter to launch Two-Speed Debug)…')
      : L('描述需求目标（Cmd+Enter 启动新功能协作）…', 'Describe feature goal (Cmd+Enter to launch Feature Pipeline)…'),
  });
  const launch = () => act(`launch:${scope}`, async () => {
    const goal = draft(goalKey).trim();
    const utt = draft(bindKey).trim();
    const proj = draft(projKey, defaultProj).trim();
    const combined = [goal, utt].filter(Boolean).join(L('；', '; '));
    if (!combined) throw new Error(isDebug ? L('请填写故障现象或排查说明', 'Please enter symptom or debug instructions') : L('请填写需求目标或协作说明', 'Please enter goal or pipeline instructions'));
    const parsed = pv?.data && pv.text === utt ? pv.data : null;
    let p;
    if (isDebug) {
      p = await api('POST', '/api/debug', {
        project_root: proj,
        symptom: goal || parsed?.goal || combined,
        utterance: utt || combined,
        auto_confirm: UI.autoConfirm,
      });
    } else {
      p = await api('POST', '/api/pipelines', {
        req: {
          playbook: 'feature',
          project_root: proj,
          goal: goal || parsed?.goal || combined,
          bindings: parsed?.bindings || {},
          source: { via: 'web', utterance: combined },
        },
        auto_confirm: UI.autoConfirm,
      });
    }
    p = upsertPipeline(p.pipeline || p);
    if (UI.autoConfirm && p.status === 'awaiting_binding_confirm') {
      p = upsertPipeline(await api('POST', `/api/pipelines/${enc(p.id)}/confirm`, {}));
    }
    UI.drafts.delete(goalKey);
    UI.drafts.delete(bindKey);
    delete UI.parsePreviews[scope];
    APP.go('pipelines', p.id);
    return p;
  }, (p) => L(`${p.id} 已启动`, `${p.id} started`));

  ta.addEventListener('keydown', (e) => {
    if (e.key === 'Enter' && (e.metaKey || e.ctrlKey) && !e.isComposing) { e.preventDefault(); launch(); }
  });
  const parsedStrip = pv?.data ? h('div', { class: 'stack-xs' },
    h('div', { class: 'row' },
      span('muted', L('解析预览：', 'Parsed bindings: ')),
      ...PLAYBOOK_STAGES[mode].map((k) => span('tag', `${stageName(k)}: ${shorthand(pv.data.resolved?.[k])}`))),
    (pv.data.warnings || []).length ? h('div', { class: 'callout warn' }, pv.data.warnings.map((w) => h('div', null, warnText(w)))) : null) : null;
  return h('div', { class: 'stack-sm' },
    h('div', { class: 'opt-panel' },
      h('div', { class: 'opt-grid' },
        h('div', null, h('label', { class: 'lbl' }, L('模型绑定话术（选填）', 'Model routing instructions (optional)')), bindInp),
        h('div', null, h('label', { class: 'lbl' }, L('项目目录（选填）', 'Project directory (optional)')), projInp)),
      parsedStrip,
      h('label', { class: 'row muted', style: 'font-size:12px;cursor:pointer' },
        h('input', { type: 'checkbox', checked: UI.autoConfirm, onchange: (e) => { UI.autoConfirm = e.target.checked; APP.schedule(); } }),
        L('跳过二次绑定确认，创建后直接启动首阶段', 'Skip confirmation step and start first stage immediately'))),
    ta,
    h('div', { class: 'composer-foot' },
      btn(L('切回直接发送', 'Back to Direct'), { cls: 'ghost sm', onclick: () => { UI.composerMode = 'direct'; APP.rerender(); } }),
      btn(isDebug ? L('启动两速查错', 'Launch Two-Speed Debug') : L('启动新功能协作', 'Launch Feature Pipeline'), { cls: 'primary sm', busyKey: `launch:${scope}`, onclick: launch })));
}
