// starfane web console — zero-build ES module entry point
import { HEALTH_POLL_MS, PREVIEW_POLL_MS, TICK_MS, TOKEN_KEY, VIEWS } from './js/constants.js';
import { $, emptyState, errorBanner, h, skeleton } from './js/dom.js';
import {
  connectSSE,
  loadAll,
  loadAllTranscript,
  loadEarlierPreview,
  loadEarlierTranscript,
  loadHandoffs,
  loadModels,
  loadPrecision,
  loadPreview,
  loadReviews,
  loadSessionOutput,
  loadSlashCommands,
  loadTmuxSessions,
  loadTranscript,
  pollActivePreview,
  pollHealth,
  refreshReview,
  renameHostApi,
  syncStateSilent,
} from './js/api.js';
import { closeSlashMenu, renderAttachBar, updateSlashFromInput } from './js/composer.js';
import {
  closeDrawer,
  closeModal,
  openDrawer,
  openRenameHostModal,
  openSpawnModal,
  renderDrawer,
  renderModal,
} from './js/overlays.js';
import {
  APP,
  S,
  UI,
  alertCount,
  currentSession,
  enc,
  pvKeyOf,
  pvModeOf,
} from './js/store.js';
import { renderTop } from './js/topbar.js';
import { viewInbox, viewPipelines, viewReviews } from './js/views.js';
import {
  ackSessionError,
  mountOrPatchWorkspace,
  notifyOutputUpdated,
  patchTelemetryStrip,
  syncScrollLatestBtn,
  updateOutputClock,
} from './js/workspace.js';

let renderTimer = 0;
let composing = false;

function schedule() {
  if (renderTimer) return;
  renderTimer = setTimeout(() => { renderTimer = 0; rerender(); }, 40);
}

function renderView() {
  const el = $('#view');
  const isWs = UI.view === 'workspace' && S.loaded;
  el.className = `view${UI.view === 'workspace' ? ' full-h' : ''}`;
  if (isWs) {
    mountOrPatchWorkspace(el);
    return;
  }
  let content;
  if (S.authNeeded && !S.loaded) {
    content = emptyState('需要认证 Token', '请在弹窗中输入本地 daemon 生成的 token，或通过 starfane web 打开控制台。');
  } else if (!S.loaded) {
    content = S.loadError
      ? errorBanner(`无法连接 starfane daemon：${S.loadError}`, '重新连接', loadAll)
      : h('div', { class: 'card' }, skeleton(6));
  } else {
    const fn = { inbox: viewInbox, pipelines: viewPipelines, reviews: viewReviews }[UI.view] || viewInbox;
    content = fn();
  }
  const nodes = [S.loaded && S.loadError ? errorBanner(`状态同步异常：${S.loadError}`, '重试同步', loadAll) : null, content].filter(Boolean);
  el.replaceChildren(...nodes);
}

function rerender() {
  if (composing) return;
  const a = document.activeElement;
  const fk = a && a.dataset ? a.dataset.draft : null;
  const sel = fk && a.selectionStart != null ? [a.selectionStart, a.selectionEnd] : null;
  const termBox = $('.term-viewport');
  const termScroll = termBox ? {
    sid: termBox.dataset.sid,
    mode: termBox.dataset.mode,
    top: termBox.scrollTop,
    atBottom: termBox.scrollHeight - termBox.scrollTop - termBox.clientHeight < 36,
  } : null;
  const drawerBody = $('.drawer-body');
  const drawerTop = drawerBody ? drawerBody.scrollTop : 0;
  const modalEl = $('#modal');
  const modalTop = modalEl?.open ? modalEl.scrollTop : 0;

  renderTop();
  renderView();
  renderDrawer();
  renderModal();

  const nextDrawer = $('.drawer-body');
  if (nextDrawer && drawerTop) nextDrawer.scrollTop = drawerTop;
  if (modalEl?.open && modalTop) modalEl.scrollTop = modalTop;
  const alerts = alertCount();
  document.title = `${alerts ? `(${alerts}) ` : ''}starfane · 控制面`;
  const nextTerm = $('.term-viewport');
  if (nextTerm && nextTerm !== termBox) {
    const samePane = termScroll && termScroll.sid === nextTerm.dataset.sid && termScroll.mode === nextTerm.dataset.mode;
    nextTerm.scrollTop = (!samePane || termScroll.atBottom) ? nextTerm.scrollHeight : termScroll.top;
    syncScrollLatestBtn(nextTerm);
  }
  if (fk) {
    const n = document.querySelector(`[data-draft="${CSS.escape(fk)}"]`);
    if (n && n !== document.activeElement) {
      n.focus({ preventScroll: true });
      if (sel) { try { n.setSelectionRange(sel[0], sel[1]); } catch { /* ignore */ } }
    }
  }
}

function go(view, arg) {
  location.hash = `#/${view}${arg ? `/${enc(arg)}` : ''}`;
}

function openSession(id) {
  UI.selSession = id;
  UI.arg = id;
  const s = S.sessions.get(id);
  if (s) loadSessionOutput(s, { silent: false });
  const wantHash = `#/workspace/${enc(id)}`;
  if (UI.view !== 'workspace' || location.hash !== wantHash) go('workspace', id);
  else rerender();
}

function route() {
  let [v, a] = (location.hash.replace(/^#\/?/, '') || 'workspace').split('/');
  if (v === 'sessions') v = 'workspace';
  if (v === 'debug') { v = 'pipelines'; UI.pipeFilter = 'debug'; }
  UI.view = VIEWS.includes(v) ? v : 'workspace';
  UI.arg = a ? decodeURIComponent(a) : null;
  if (UI.view === 'workspace' && UI.arg && S.sessions.has(UI.arg)) UI.selSession = UI.arg;
  if (UI.view === 'reviews') {
    if (S.reviewsState === 'idle' || S.reviewsState === 'err') loadReviews();
    if (S.precisionState === 'idle') loadPrecision();
  }
  const s = currentSession();
  if (UI.view === 'workspace' && s) {
    if (s.agent !== 'shell' && !S.transcripts.has(s.session_id)) loadTranscript(s.session_id, { silent: false });
    if (s.pane && !S.previews.has(pvKeyOf(s.pane))) loadPreview(s.pane, { mode: pvModeOf(s.pane), silent: false });
  }
  schedule();
}

// Wire cross-module callback registry before boot.
Object.assign(APP, {
  schedule,
  rerender,
  go,
  openSession,
  openDrawer,
  closeDrawer,
  openSpawnModal,
  openRenameHostModal,
  closeModal,
  notifyOutputUpdated,
  updateOutputClock,
  patchTelemetryStrip,
  loadAll,
  syncStateSilent,
  loadSessionOutput,
  loadTranscript,
  loadEarlierTranscript,
  loadAllTranscript,
  loadPreview,
  loadEarlierPreview,
  loadModels,
  loadTmuxSessions,
  loadSlashCommands,
  loadHandoffs,
  loadReviews,
  refreshReview,
  loadPrecision,
  connectSSE,
  renameHost: renameHostApi,
  updateSlashFromInput,
  renderAttachBar,
  closeSlashMenu,
  ackSessionError,
});

// Global listeners.
document.addEventListener('compositionstart', () => { composing = true; });
document.addEventListener('compositionend', () => { composing = false; schedule(); });
document.addEventListener('toggle', (e) => {
  const d = e.target;
  if (d instanceof HTMLDetailsElement && d.dataset.k) {
    if (d.open) {
      UI.open.add(d.dataset.k);
      if (d.classList.contains('live-tail')) {
        const cur = currentSession();
        if (cur?.pane) loadPreview(cur.pane, { mode: pvModeOf(cur.pane), silent: true });
      }
    } else UI.open.delete(d.dataset.k);
  }
}, true);
document.addEventListener('click', (e) => {
  if (UI.muteMenu && !e.target.closest('.mute-wrap')) { UI.muteMenu = false; schedule(); }
  if (UI.slash.open && !e.target.closest('.composer-card')) closeSlashMenu();
});
window.addEventListener('hashchange', route);
document.addEventListener('keydown', (e) => {
  if (e.key === 'Escape' && UI.muteMenu) { UI.muteMenu = false; schedule(); return; }
  if (e.key === 'Escape' && UI.drawer) { closeDrawer(); return; }
  if (e.metaKey || e.ctrlKey || e.altKey) return;
  const t = e.target;
  if (t && (/^(INPUT|TEXTAREA|SELECT)$/.test(t.tagName) || t.isContentEditable)) return;
  if (UI.modal || S.authNeeded) return;
  const n = Number(e.key);
  if (n >= 1 && n <= VIEWS.length) go(VIEWS[n - 1]);
  else if (e.key === 'n') { openSpawnModal(); e.preventDefault(); }
});
const modalEl = $('#modal');
modalEl.addEventListener('close', () => { if (!S.authNeeded) UI.modal = null; });
modalEl.addEventListener('cancel', (e) => { if (S.authNeeded) e.preventDefault(); });
modalEl.addEventListener('click', (e) => { if (e.target.id === 'modal' && !S.authNeeded) closeModal(); });

async function boot() {
  const u = new URL(location.href);
  const tk = u.searchParams.get('token');
  if (tk) {
    localStorage.setItem(TOKEN_KEY, tk);
    u.searchParams.delete('token');
    history.replaceState(null, '', u.pathname + u.search + u.hash);
  }
  route();
  rerender();
  await pollHealth();
  await loadAll();
  connectSSE();
  loadModels();
  setInterval(pollHealth, HEALTH_POLL_MS);
  setInterval(pollActivePreview, PREVIEW_POLL_MS);
  setInterval(() => {
    syncStateSilent();
    const sel = window.getSelection?.();
    if (sel && !sel.isCollapsed && $('.term-viewport')?.contains(sel.anchorNode)) return;
    schedule();
  }, TICK_MS);
  document.addEventListener('visibilitychange', () => { if (!document.hidden) syncStateSilent(); });
}

boot();
