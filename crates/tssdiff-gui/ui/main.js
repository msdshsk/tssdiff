'use strict';

/* Surface unexpected JS errors instead of failing silently */
window.__errors = [];
window.addEventListener('error', (e) => {
  window.__errors.push(String(e.message));
  try { toast('内部エラー: ' + e.message); } catch (_) { /* toast not ready */ }
});
window.addEventListener('unhandledrejection', (e) => {
  window.__errors.push(String(e.reason));
  try { toast('内部エラー: ' + e.reason); } catch (_) { /* toast not ready */ }
});

const { invoke } = window.__TAURI__.core;
const appWindow = window.__TAURI__.window.getCurrentWindow();

/* ---------- state ---------- */
const state = {
  repo: null,        // { root, branch }
  mode: 'working',   // 'working' | 'staged' | 'commit:<hash>'
  historyMode: false,
  commits: [],       // [{ hash, date, subject }]
  files: [],         // [{ path, added, removed }]
  current: null,     // selected file path
  rows: [],          // aligned rows of the current diff
  afterOnly: false,
  filter: '',
  collapsedDirs: new Set(),
  expandedFolds: new Set(),
  sel: { start: null, end: null },
  notes: [],           // NoteOut list for the current file
  awaiting: new Set(), // question ids still waiting for a reply
  lastReplies: 0,
};

/* Keep context lines around changes; fold longer runs (mirrors core) */
const FOLD_CONTEXT = 3;

const $ = (id) => document.getElementById(id);
const diffBody = $('diffBody');

/* ---------- toast ---------- */
let toastTimer = null;
function toast(msg) {
  const el = $('toast');
  el.textContent = msg;
  el.classList.add('show');
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => el.classList.remove('show'), 2800);
}

/* ---------- theme ---------- */
const darkQuery = window.matchMedia('(prefers-color-scheme: dark)');
function syntectTheme() {
  return darkQuery.matches ? 'base16-ocean.dark' : 'InspiredGitHub';
}
darkQuery.addEventListener('change', () => {
  if (state.current) loadDiff(state.current);
});

/* ---------- repo ---------- */
async function tryOpen(path, silent) {
  try {
    const info = await invoke('open_repo', { path });
    state.repo = info;
    state.current = null;
    state.expandedFolds.clear();
    $('repoChip').hidden = false;
    $('repoLabel').textContent = info.root + ' · ' + info.branch;
    $('sbBranch').hidden = false;
    $('sbBranchName').textContent = info.branch;
    $('sbBackend').hidden = false;
    $('sbBackend').textContent = info.backend === 'gix' ? 'via gix (built-in)' : 'via git';
    $('sbWatch').hidden = !info.watching;
    const name = info.root.split(/[\\/]/).pop();
    appWindow.setTitle(name + ' — tssdiff');
    state.commits = [];
    await switchTab('working');
  } catch (e) {
    if (!silent) toast(String(e));
    showWelcome();
  }
}

async function pickRepo() {
  const dir = await invoke('plugin:dialog|open', {
    options: { directory: true, title: 'git リポジトリを開く' },
  });
  if (dir) tryOpen(dir, false);
}

function showWelcome() {
  $('emptyState').classList.add('show');
  $('emptyTitle').textContent = 'リポジトリを開いてください';
  $('emptyBody').innerHTML =
    'Ctrl+O、または右上のフォルダボタンから git リポジトリを選択します。<br>' +
    'コマンドラインからは <code>tssdiff-gui &lt;path&gt;</code> で直接開けます。';
  $('emptyOpen').hidden = false;
  $('fileHead').hidden = true;
  diffBody.innerHTML = '';
}

function showNoChanges() {
  $('emptyState').classList.add('show');
  $('emptyTitle').textContent = '変更はありません';
  $('emptyBody').textContent = state.mode.startsWith('commit:')
    ? 'このコミットに変更はありません。'
    : state.mode === 'staged'
      ? 'ステージされた変更がありません。'
      : 'ワーキングツリーはクリーンです。';
  $('emptyOpen').hidden = true;
  $('fileHead').hidden = true;
  diffBody.innerHTML = '';
}

function showBinary(path) {
  const f = state.files.find((x) => x.path === path);
  $('fileHead').hidden = false;
  $('fhPath').textContent = path;
  $('fhStat').innerHTML = f ? `<span class="a">+${f.added}</span> <span class="d">−${f.removed}</span>` : '';
  $('emptyState').classList.add('show');
  $('emptyTitle').textContent = 'バイナリファイル';
  $('emptyBody').textContent = 'テキストとして表示できないため、差分表示をスキップしました。';
  $('emptyOpen').hidden = true;
  diffBody.innerHTML = '';
}

/* ---------- file list / tree ---------- */
/// soft: keep the current file, scroll position, folds, and selection
/// (used by watch/F5 refreshes so external edits don't yank the view)
async function loadFiles(soft) {
  try {
    state.files = await invoke('load_files', { mode: state.mode });
  } catch (e) {
    toast(String(e));
    state.files = [];
  }
  renderTree();

  const totalA = state.files.reduce((s, f) => s + f.added, 0);
  const totalD = state.files.reduce((s, f) => s + f.removed, 0);
  $('treeStat').innerHTML = state.files.length
    ? `${state.files.length} files <span class="a">+${totalA}</span> <span class="d">−${totalD}</span>`
    : '';

  if (!state.files.length) {
    state.current = null;
    showNoChanges();
    updateStatus();
    return;
  }
  const keep = state.files.find((f) => f.path === state.current);
  if (keep && soft) {
    await loadDiff(keep.path, true);
    updateStatus();
    return;
  }
  selectFile(keep ? keep.path : state.files[0].path);
}

/* Build a nested dir tree from flat paths, honoring collapsed dirs */
function renderTree() {
  const list = $('treeList');
  const filter = state.filter.toLowerCase();
  const files = filter
    ? state.files.filter((f) => f.path.toLowerCase().includes(filter))
    : state.files;

  if (!files.length) {
    list.innerHTML = '<div class="tree-empty">' +
      (state.files.length ? '絞り込みに一致するファイルがありません' : '変更されたファイルはありません') +
      '</div>';
    return;
  }

  const frag = document.createDocumentFragment();
  const seenDirs = new Set();
  for (const f of files) {
    const parts = f.path.split('/');
    let hidden = false;
    for (let d = 0; d < parts.length - 1; d++) {
      const dirPath = parts.slice(0, d + 1).join('/');
      if (!seenDirs.has(dirPath)) {
        seenDirs.add(dirPath);
        if (!hidden) {
          const el = document.createElement('div');
          el.className = 'titem dir';
          el.style.paddingLeft = 14 + d * 14 + 'px';
          el.dataset.dir = dirPath;
          el.innerHTML = `<span class="fname">${state.collapsedDirs.has(dirPath) ? '▸' : '▾'} ${esc(parts[d])}/</span>`;
          frag.appendChild(el);
        }
      }
      if (state.collapsedDirs.has(dirPath)) hidden = true;
    }
    if (hidden) continue;
    const depth = parts.length - 1;
    const el = document.createElement('div');
    el.className = 'titem' + (f.path === state.current ? ' active' : '');
    el.style.paddingLeft = 14 + depth * 14 + 'px';
    el.dataset.file = f.path;
    el.setAttribute('role', 'button');
    el.tabIndex = 0;
    el.innerHTML =
      `<span class="fname">${esc(parts[depth])}</span>` +
      `<span class="fstat"><span class="a">+${f.added}</span> <span class="d">−${f.removed}</span></span>`;
    frag.appendChild(el);
  }
  list.innerHTML = '';
  list.appendChild(frag);
}

$('treeList').addEventListener('click', (e) => {
  const dir = e.target.closest('.titem.dir');
  if (dir) {
    const p = dir.dataset.dir;
    state.collapsedDirs.has(p) ? state.collapsedDirs.delete(p) : state.collapsedDirs.add(p);
    renderTree();
    return;
  }
  const item = e.target.closest('.titem[data-file]');
  if (item) selectFile(item.dataset.file);
});

function esc(s) {
  return s.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;');
}

/* ---------- diff ---------- */
async function selectFile(path) {
  state.current = path;
  state.expandedFolds.clear();
  clearSelection();
  renderTree();
  await loadDiff(path);
  updateStatus();
}

async function loadDiff(path, keepScroll) {
  let out;
  try {
    out = await invoke('load_diff', { mode: state.mode, path, theme: syntectTheme() });
  } catch (e) {
    toast(String(e));
    return;
  }
  if (out.binary) {
    state.rows = [];
    state.notes = [];
    showBinary(path);
    return;
  }
  state.rows = out.rows;
  state.notes = out.notes;
  updateAwaiting();
  const f = state.files.find((x) => x.path === path);
  $('emptyState').classList.remove('show');
  $('fileHead').hidden = false;
  $('fhPath').textContent = path;
  $('fhStat').innerHTML = f ? `<span class="a">+${f.added}</span> <span class="d">−${f.removed}</span>` : '';
  renderDiff();
  if (!keepScroll) $('diffScroll').scrollTop = 0;
}

/* Display list: rows kept near changes, long context runs folded */
function displayList() {
  const rows = state.rows;
  if (!rows.length) return [];
  const keep = new Array(rows.length).fill(false);
  // rows with notes stay visible, like changed rows
  const noteRows = new Set(
    state.notes.map((n) => n.row).filter((r) => r !== null && r !== undefined)
  );
  let hasChange = false;
  rows.forEach((r, i) => {
    if (r.kind !== 'ctx' || noteRows.has(i)) {
      hasChange = true;
      const s = Math.max(0, i - FOLD_CONTEXT);
      const e = Math.min(rows.length, i + FOLD_CONTEXT + 1);
      keep.fill(true, s, e);
    }
  });
  if (!hasChange) return rows.map((_, i) => ({ row: i }));

  const out = [];
  let i = 0;
  while (i < rows.length) {
    if (keep[i] || state.expandedFolds.has(foldStartOf(keep, i))) {
      out.push({ row: i });
      i++;
    } else {
      const start = i;
      while (i < rows.length && !keep[i]) i++;
      const hidden = i - start;
      if (hidden <= 2 || state.expandedFolds.has(start)) {
        for (let r = start; r < i; r++) out.push({ row: r });
      } else {
        out.push({ fold: start, hidden });
      }
    }
  }
  return out;
}

function foldStartOf(keep, i) {
  // walk back to the start of this unkept run
  let s = i;
  while (s > 0 && !keep[s - 1]) s--;
  return s;
}

function segsHtml(segs) {
  return segs
    .map((s) => (s.c ? `<span style="color:${s.c}">${esc(s.t)}</span>` : esc(s.t)))
    .join('');
}

/// One visual block stacking every note anchored to the same row
function noteBlock(notes) {
  const wrap = document.createElement('div');
  wrap.className = 'note-block';
  for (const n of notes) {
    const entry = document.createElement('div');
    entry.className = 'note-entry';
    const who =
      n.author === 'you'
        ? '<span class="you">あなた</span>'
        : `<span class="agent-dot"></span>${esc(n.author)}`;
    const line =
      n.new_line != null ? `新 ${n.new_line} 行` : n.old_line != null ? `旧 ${n.old_line} 行` : '';
    const long = n.body.split('\n').length > 4 || n.body.length > 320;
    entry.innerHTML =
      `<div class="who">${who}<span>· ${line}</span></div>` +
      `<div class="nbody${long ? ' folded' : ''}">${esc(n.body)}</div>` +
      (long ? '<button class="note-more">すべて表示</button>' : '');
    wrap.appendChild(entry);
  }
  const awaiting = notes.some((n) => n.author === 'you' && state.awaiting.has(n.reply_to));
  if (awaiting) {
    const p = document.createElement('div');
    p.className = 'note-pending';
    p.innerHTML = '<span class="pulse"></span>エージェントへ送信済み — 返信待ち';
    wrap.appendChild(p);
  }
  return wrap;
}

function updateAwaiting() {
  for (const n of state.notes) {
    if (n.author !== 'you' && n.reply_to) state.awaiting.delete(n.reply_to);
  }
}

function updateNotesBadge(sent, replies) {
  const el = $('sbNotes');
  el.hidden = !(sent + replies);
  el.textContent = `notes ${sent} · replies ${replies}`;
}

function renderDiff() {
  const notesByRow = new Map();
  const orphans = [];
  for (const n of state.notes) {
    if (n.row === null || n.row === undefined) {
      orphans.push(n);
      continue;
    }
    if (!notesByRow.has(n.row)) notesByRow.set(n.row, []);
    notesByRow.get(n.row).push(n);
  }

  const frag = document.createDocumentFragment();
  if (orphans.length) frag.appendChild(noteBlock(orphans));
  for (const entry of displayList()) {
    if (entry.fold !== undefined) {
      const el = document.createElement('div');
      el.className = 'fold-row';
      el.dataset.fold = entry.fold;
      el.innerHTML = `<span class="rule"></span>⋯ ${entry.hidden} 行の変更なし(クリックで展開)<span class="rule"></span>`;
      frag.appendChild(el);
      continue;
    }
    const i = entry.row;
    const r = state.rows[i];
    const el = document.createElement('div');
    el.className = 'drow';
    el.dataset.idx = i;

    if (state.afterOnly) {
      if (r.new_no == null) continue;
      const changed = r.kind === 'add' || r.kind === 'mod';
      el.innerHTML =
        `<div class="gut${changed ? ' gadd' : ''}" data-idx="${i}">${r.new_no}</div>` +
        `<div class="dcell new-side${changed ? ' add-line' : ''}">${segsHtml(r.new)}</div>`;
    } else {
      const oGut = r.kind === 'del' || r.kind === 'mod' ? ' gdel' : '';
      const nGut = r.kind === 'add' || r.kind === 'mod' ? ' gadd' : '';
      const oCls = r.kind === 'del' || r.kind === 'mod' ? ' del-line' : '';
      const nCls = r.kind === 'add' || r.kind === 'mod' ? ' add-line' : '';
      el.innerHTML =
        `<div class="gut${r.old_no != null ? oGut : ''}" data-idx="${i}">${r.old_no ?? ''}</div>` +
        `<div class="dcell old-side${r.old ? oCls : ''}">${r.old ? segsHtml(r.old) : ''}</div>` +
        `<div class="gut${r.new_no != null ? nGut : ''}" data-idx="${i}">${r.new_no ?? ''}</div>` +
        `<div class="dcell new-side${r.new ? nCls : ''}">${r.new ? segsHtml(r.new) : ''}</div>`;
    }
    frag.appendChild(el);
    const anchored = notesByRow.get(i);
    if (anchored) frag.appendChild(noteBlock(anchored));
  }
  const scroller = $('diffScroll');
  const scrollTop = scroller.scrollTop;
  diffBody.innerHTML = '';
  diffBody.appendChild(frag);
  diffBody.parentElement.classList.toggle('after-only', state.afterOnly);
  scroller.scrollTop = scrollTop;
  applySelection();
}

diffBody.addEventListener('click', (e) => {
  const more = e.target.closest('.note-more');
  if (more) {
    const body = more.parentElement.querySelector('.nbody');
    const folded = body.classList.toggle('folded');
    more.textContent = folded ? 'すべて表示' : '折りたたむ';
    return;
  }
  const fold = e.target.closest('.fold-row');
  if (fold) {
    state.expandedFolds.add(Number(fold.dataset.fold));
    renderDiff();
    return;
  }
  const gut = e.target.closest('.gut');
  if (gut) {
    const idx = Number(gut.dataset.idx);
    if (e.shiftKey && state.sel.start !== null) state.sel.end = idx;
    else { state.sel.start = idx; state.sel.end = idx; }
    hidePopover();
    applySelection();
  }
});

/* ---------- selection (feedback lands in Phase 2) ---------- */
function selRange() {
  if (state.sel.start === null) return null;
  return [Math.min(state.sel.start, state.sel.end), Math.max(state.sel.start, state.sel.end)];
}

function applySelection() {
  const range = selRange();
  diffBody.querySelectorAll('.drow').forEach((el) => {
    const idx = Number(el.dataset.idx);
    el.classList.toggle('selected', !!range && idx >= range[0] && idx <= range[1]);
  });
  positionFloat();
}

function clearSelection() {
  state.sel.start = state.sel.end = null;
  $('popover').hidden = true;
  applySelection();
}

function lastSelectedRowEl() {
  const rows = diffBody.querySelectorAll('.drow.selected');
  return rows.length ? rows[rows.length - 1] : null;
}

function positionFloat() {
  const btn = $('floatComment');
  const last = lastSelectedRowEl();
  if (!last || !$('popover').hidden) {
    btn.hidden = true;
    return;
  }
  btn.hidden = false;
  btn.style.top = last.offsetTop + last.offsetHeight + 4 + 'px';
  btn.style.left = '64px';
}

function selectedLineLabel() {
  const range = selRange();
  if (!range) return '';
  const span = (key, prefix) => {
    const nums = [];
    for (let i = range[0]; i <= range[1]; i++) {
      const r = state.rows[i];
      if (r && r[key] != null) nums.push(r[key]);
    }
    if (!nums.length) return '';
    return `${prefix} ${nums[0]}${nums.length > 1 ? '–' + nums[nums.length - 1] : ''} 行`;
  };
  return span('new_no', '新') || span('old_no', '旧');
}

/* ---------- feedback popover ---------- */
let popKind = 'comment';

function openPopover() {
  const range = selRange();
  if (!range) {
    toast('先に行番号をクリックして範囲を選択してください');
    return;
  }
  const last = lastSelectedRowEl();
  const pop = $('popover');
  $('popLines').textContent = (state.current || '') + ' · ' + selectedLineLabel();
  pop.hidden = false;
  pop.style.top = (last ? last.offsetTop + last.offsetHeight + 4 : 40) + 'px';
  pop.style.left = '64px';
  $('floatComment').hidden = true;
  $('popText').value = '';
  $('popText').focus();
  requestAnimationFrame(() => pop.scrollIntoView({ block: 'nearest', behavior: 'smooth' }));
}

function hidePopover() {
  $('popover').hidden = true;
  positionFloat();
  if (pendingRefresh) {
    pendingRefresh = false;
    refreshAll();
  }
}

function setKind(kind) {
  popKind = kind;
  document.querySelectorAll('#kindSeg button').forEach((b) =>
    b.setAttribute('aria-pressed', String(b.dataset.kind === kind))
  );
}

function toggleKind() {
  setKind(popKind === 'comment' ? 'question' : 'comment');
}

async function sendFeedback() {
  const text = $('popText').value.trim();
  if (!text) {
    $('popText').focus();
    return;
  }
  const range = selRange();
  if (!range) return;
  $('popSend').disabled = true;
  try {
    const out = await invoke('send_feedback', {
      kind: popKind,
      comment: text,
      selStart: range[0],
      selEnd: range[1],
    });
    if (popKind === 'question') state.awaiting.add(out.id);
    state.notes = out.notes;
    state.lastReplies = out.replies;
    updateNotesBadge(out.sent, out.replies);
    hidePopover();
    clearSelection();
    renderDiff();
    toast('送信しました: ' + out.status);
  } catch (e) {
    toast(String(e));
  } finally {
    $('popSend').disabled = false;
  }
}

let threadCursor = -1;
function jumpNextThread() {
  const blocks = Array.from(diffBody.querySelectorAll('.note-block'));
  if (!blocks.length) {
    toast('注釈スレッドはありません');
    return;
  }
  threadCursor = (threadCursor + 1) % blocks.length;
  blocks[threadCursor].scrollIntoView({ block: 'center', behavior: 'smooth' });
}

$('floatComment').addEventListener('click', openPopover);
$('popSend').addEventListener('click', sendFeedback);
$('kindSeg').addEventListener('click', (e) => {
  const b = e.target.closest('button[data-kind]');
  if (b) setKind(b.dataset.kind);
});
$('popText').addEventListener('keydown', (e) => {
  if (e.key === 'Enter' && e.ctrlKey) {
    e.preventDefault();
    sendFeedback();
  } else if (e.key === 'Tab') {
    e.preventDefault();
    toggleKind();
  }
});

/* ---------- context menu ---------- */
const ctxMenu = $('ctxMenu');

function hideCtxMenu() {
  ctxMenu.hidden = true;
  ctxMenu.innerHTML = '';
}

function showCtxMenu(items, x, y) {
  ctxMenu.innerHTML = '';
  for (const it of items) {
    if (it === '-') {
      const sep = document.createElement('div');
      sep.className = 'ctx-sep';
      ctxMenu.appendChild(sep);
      continue;
    }
    const btn = document.createElement('button');
    btn.className = 'ctx-item';
    btn.disabled = !!it.disabled;
    btn.setAttribute('role', 'menuitem');
    btn.innerHTML = `<span>${esc(it.label)}</span>` + (it.kbd ? `<kbd>${esc(it.kbd)}</kbd>` : '');
    btn.addEventListener('click', () => {
      hideCtxMenu();
      it.action();
    });
    ctxMenu.appendChild(btn);
  }
  ctxMenu.style.left = x + 'px';
  ctxMenu.style.top = y + 'px';
  ctxMenu.hidden = false;
  const rect = ctxMenu.getBoundingClientRect();
  ctxMenu.style.left = Math.max(4, Math.min(x, window.innerWidth - rect.width - 8)) + 'px';
  ctxMenu.style.top = Math.max(4, Math.min(y, window.innerHeight - rect.height - 8)) + 'px';
}

async function copyText(text, what) {
  try {
    await invoke('copy_text', { text });
    toast(what + ' をコピーしました');
  } catch (e) {
    toast(String(e));
  }
}

function openInEditor(path) {
  if (!path) return;
  invoke('open_in_editor', { path })
    .then((program) => toast(program.split(/[\\/]/).pop() + ' で開きました'))
    .catch((e) => toast(String(e)));
}

/* ---------- editor picker: detect installed editors on first use ---------- */
let pendingEditorPath = null;

async function ensureEditorThen(path) {
  try {
    const status = await invoke('editor_status');
    if (status.configured) {
      openInEditor(path);
      return;
    }
    pendingEditorPath = path;
    showEditorPicker(status.candidates);
  } catch (e) {
    toast(String(e));
  }
}

function showEditorPicker(candidates) {
  const box = $('editorChoices');
  box.innerHTML = '';
  for (const c of candidates) {
    const btn = document.createElement('button');
    btn.className = 'editor-choice';
    btn.innerHTML = `<span>${esc(c.label)}</span><span class="cmd">${esc(c.command)}</span>`;
    btn.addEventListener('click', () => chooseEditor(c.command));
    box.appendChild(btn);
  }
  $('editorOverlay').classList.add('open');
}

async function chooseEditor(command) {
  try {
    await invoke('set_editor', { command });
    $('editorOverlay').classList.remove('open');
    toast('エディタを設定しました');
    if (pendingEditorPath) {
      const path = pendingEditorPath;
      pendingEditorPath = null;
      openInEditor(path);
    }
  } catch (e) {
    toast(String(e));
  }
}

function closeEditorPicker() {
  pendingEditorPath = null;
  $('editorOverlay').classList.remove('open');
}

$('editorClose').addEventListener('click', closeEditorPicker);
$('editorOverlay').addEventListener('click', (e) => {
  if (e.target === $('editorOverlay')) closeEditorPicker();
});
$('editorBrowse').addEventListener('click', async () => {
  const picked = await invoke('plugin:dialog|open', {
    options: {
      title: 'エディタの実行ファイルを選択',
      filters: [{ name: '実行ファイル', extensions: ['exe', 'cmd', 'bat'] }],
    },
  });
  if (picked) chooseEditor('"' + picked + '"');
});

function diffRowMenu(drow) {
  const idx = Number(drow.dataset.idx);
  const r = state.rows[idx];
  if (!r) return generalMenu();
  const range = selRange();
  const inSel = range && idx >= range[0] && idx <= range[1];
  const lineNo = r.new_no ?? r.old_no;
  const lineText = (r.new || r.old || []).map((s) => s.t).join('');
  const osSelection = String(window.getSelection() || '');
  const items = [
    {
      label: inSel && range[0] !== range[1] ? '選択範囲にコメント…' : 'この行にコメント…',
      kbd: 'C',
      action: () => {
        if (!inSel) {
          state.sel = { start: idx, end: idx };
          applySelection();
        }
        openPopover();
      },
    },
    '-',
  ];
  if (osSelection.trim()) {
    items.push({ label: '選択テキストをコピー', action: () => copyText(osSelection, 'テキスト') });
  }
  items.push({ label: '行テキストをコピー', action: () => copyText(lineText, '行') });
  items.push({
    label: `${state.current}:${lineNo} をコピー`,
    action: () => copyText(`${state.current}:${lineNo}`, '位置'),
  });
  items.push('-');
  items.push({ label: 'エディタで開く', action: () => ensureEditorThen(state.current) });
  if (range) items.push({ label: '選択を解除', kbd: 'Esc', action: clearSelection });
  return items;
}

function fileMenu(path) {
  return [
    { label: 'diff を表示', action: () => selectFile(path) },
    { label: 'エディタで開く', action: () => ensureEditorThen(path) },
    '-',
    { label: '相対パスをコピー', action: () => copyText(path, 'パス') },
    {
      label: 'エクスプローラーで表示',
      action: () => invoke('reveal_in_explorer', { path }).catch((e) => toast(String(e))),
    },
  ];
}

function commitMenu(citem) {
  const hash = citem.dataset.commit;
  if (!hash) return generalMenu();
  const subject = (state.commits.find((c) => c.hash === hash) || {}).subject || '';
  return [
    { label: 'このコミットを表示', action: () => citem.click() },
    '-',
    { label: `ハッシュ ${hash} をコピー`, action: () => copyText(hash, 'ハッシュ') },
    { label: '件名をコピー', action: () => copyText(subject, '件名') },
  ];
}

function generalMenu() {
  return [
    { label: '更新', kbd: 'F5', action: refreshAll, disabled: !state.repo },
    {
      label: state.afterOnly ? 'Side-by-side 表示に切替' : 'After-only 表示に切替',
      kbd: state.afterOnly ? 'Ctrl+1' : 'Ctrl+2',
      action: () => setView(!state.afterOnly),
      disabled: !state.current,
    },
    {
      label: 'ファイルツリーの表示/非表示',
      kbd: 'Ctrl+B',
      action: () => $('treePane').classList.toggle('hidden'),
    },
    '-',
    { label: 'リポジトリを開く…', kbd: 'Ctrl+O', action: pickRepo },
    {
      label: 'エディタの設定…',
      action: async () => {
        pendingEditorPath = null;
        try {
          showEditorPicker((await invoke('editor_status')).candidates);
        } catch (e) {
          toast(String(e));
        }
      },
      disabled: !state.repo,
    },
    { label: 'ショートカット一覧', kbd: '?', action: openHelp },
  ];
}

function menuItemsFor(e) {
  const t = e.target;
  const note = t.closest('.note-entry');
  if (note) {
    const body = (note.querySelector('.nbody') || {}).textContent || '';
    return [{ label: '注釈をコピー', action: () => copyText(body, '注釈') }];
  }
  const drow = t.closest('.drow');
  if (drow) return diffRowMenu(drow);
  const citem = t.closest('.citem');
  if (citem) return commitMenu(citem);
  const titem = t.closest('.titem[data-file]');
  if (titem) return fileMenu(titem.dataset.file);
  return generalMenu();
}

document.addEventListener('contextmenu', (e) => {
  const t = e.target;
  // native menu stays available inside text inputs (paste, IME)
  if (t.closest('input, textarea')) return;
  e.preventDefault();
  if (t.closest('.ctxmenu')) return;
  hideCtxMenu();
  const items = menuItemsFor(e);
  if (items.length) showCtxMenu(items, e.clientX, e.clientY);
});

document.addEventListener('click', (e) => {
  if (!e.target.closest('.ctxmenu')) hideCtxMenu();
});
window.addEventListener('blur', hideCtxMenu);
window.addEventListener('resize', hideCtxMenu);
$('diffScroll').addEventListener('scroll', hideCtxMenu);

/* ---------- reply polling (Q4: inline + badge + in-app toast) ---------- */
setInterval(async () => {
  if (!state.repo) return;
  try {
    const out = await invoke('poll_notes');
    updateNotesBadge(out.sent, out.replies);
    if (out.replies > state.lastReplies) {
      state.lastReplies = out.replies;
      state.notes = out.notes;
      updateAwaiting();
      renderDiff();
      toast('エージェントから返信が届きました');
    }
  } catch (_) {
    /* repo may be mid-switch; next tick recovers */
  }
}, 2000);

/* ---------- modes / views ---------- */
function modeLabel() {
  if (state.mode.startsWith('commit:')) return 'History · ' + state.mode.slice(7);
  if (state.historyMode) return 'History · Working tree';
  return state.mode === 'staged' ? 'Staged' : 'Working Tree';
}

async function switchTab(tab) {
  document.querySelectorAll('#modeTabs button').forEach((b) =>
    b.setAttribute('aria-selected', String(b.dataset.mode === tab))
  );
  state.historyMode = tab === 'history';
  $('commitPane').hidden = !state.historyMode;
  $('filesHeadLabel').classList.remove('hash');
  if (state.historyMode) {
    state.mode = 'working';
    $('filesHeadLabel').textContent = 'Files';
    if (state.repo) await loadCommits();
  } else {
    state.mode = tab;
    $('filesHeadLabel').textContent = 'Changes';
    renderCommits();
  }
  $('sbMode').textContent = modeLabel();
  if (state.repo) loadFiles();
}

$('modeTabs').addEventListener('click', (e) => {
  const btn = e.target.closest('button[data-mode]');
  if (btn) switchTab(btn.dataset.mode);
});

/* ---------- history (commit list) ---------- */
async function loadCommits() {
  try {
    state.commits = await invoke('load_commits');
  } catch (e) {
    toast(String(e));
    state.commits = [];
  }
  $('commitStat').textContent = state.commits.length
    ? `${state.commits.length}${state.commits.length >= 300 ? '+' : ''} commits`
    : '';
  renderCommits();
}

/* Lane assignment for the commit graph: each lane carries the hash it
   expects next; commits claim their lane, merges close lanes, extra
   parents fork new ones */
const LANE_COLORS = ['#b98a2e', '#4e9a83', '#a86b9e', '#5f81b5', '#a08048', '#7d9a4e'];
const LANE_STEP = 9;

function laneX(i) {
  return 5 + i * LANE_STEP;
}

function computeGraph(commits) {
  const lanes = [];
  const rows = [];
  const seen = new Set();
  let maxLanes = 1;
  for (const c of commits) {
    seen.add(c.hash);
    const mine = [];
    lanes.forEach((h, i) => {
      if (h === c.hash) mine.push(i);
    });
    let col;
    if (mine.length) col = mine[0];
    else {
      col = lanes.indexOf(null);
      if (col < 0) {
        col = lanes.length;
        lanes.push(null);
      }
    }
    const pre = lanes.slice();
    const merged = mine.slice(1);
    merged.forEach((i) => (lanes[i] = null));
    // parents already drawn above (log order anomaly) get no edge
    // rather than a line dangling to the bottom of the list
    const parents = (c.parents || []).filter((p) => !seen.has(p));
    lanes[col] = parents[0] || null;
    const forks = [];
    for (const p of parents.slice(1)) {
      let t = lanes.findIndex((h, i) => h === p && i !== col);
      if (t < 0) {
        t = lanes.indexOf(null);
        if (t < 0) {
          t = lanes.length;
          lanes.push(null);
        }
        lanes[t] = p;
      }
      forks.push(t);
    }
    rows.push({ col, mine, merged, forks, pre, post: lanes.slice() });
    maxLanes = Math.max(maxLanes, lanes.length);
    while (lanes.length && lanes[lanes.length - 1] === null) lanes.pop();
  }
  return { rows, maxLanes };
}

function svgGraph(r, width) {
  const color = (i) => LANE_COLORS[i % LANE_COLORS.length];
  const xc = laneX(r.col);
  const parts = [];
  r.pre.forEach((h, l) => {
    if (h != null && !r.mine.includes(l)) {
      parts.push(`<path d="M${laneX(l)} 0 V40" stroke="${color(l)}"/>`);
    }
  });
  if (r.mine.length) parts.push(`<path d="M${xc} 0 V20" stroke="${color(r.col)}"/>`);
  for (const m of r.merged) {
    const xm = laneX(m);
    parts.push(`<path d="M${xm} 0 C ${xm} 14, ${xc} 6, ${xc} 20" stroke="${color(m)}"/>`);
  }
  if (r.post[r.col] != null) parts.push(`<path d="M${xc} 20 V40" stroke="${color(r.col)}"/>`);
  for (const t of r.forks) {
    const xt = laneX(t);
    parts.push(`<path d="M${xc} 20 C ${xc} 34, ${xt} 26, ${xt} 40" stroke="${color(t)}"/>`);
  }
  parts.push(`<circle cx="${xc}" cy="20" r="3.5" fill="${color(r.col)}" stroke="none"/>`);
  return (
    `<svg class="cgraph" width="${width}" height="40" viewBox="0 0 ${width} 40" ` +
    `fill="none" stroke-width="1.6">${parts.join('')}</svg>`
  );
}

function renderCommits() {
  const list = $('commitList');
  if (!state.historyMode) {
    list.innerHTML = '';
    return;
  }
  const selected = state.mode.startsWith('commit:') ? state.mode.slice(7) : null;
  const { rows, maxLanes } = computeGraph(state.commits);
  const width = 8 + Math.min(maxLanes, 8) * LANE_STEP;
  const frag = document.createDocumentFragment();

  const wt = document.createElement('div');
  wt.className = 'citem wt' + (selected === null ? ' active' : '');
  wt.dataset.commit = '';
  wt.setAttribute('role', 'button');
  wt.tabIndex = 0;
  wt.innerHTML =
    `<svg class="cgraph" width="${width}" height="40" viewBox="0 0 ${width} 40" fill="none" stroke-width="1.6">` +
    `<circle cx="${laneX(0)}" cy="20" r="3.5" stroke="var(--add)"/>` +
    `<path d="M${laneX(0)} 24 V40" stroke="var(--add)" stroke-dasharray="2 3"/></svg>` +
    '<div class="cbody"><div class="crow"><span class="csubj">Working tree</span></div></div>';
  frag.appendChild(wt);

  state.commits.forEach((c, i) => {
    const el = document.createElement('div');
    el.className = 'citem' + (selected === c.hash ? ' active' : '');
    el.dataset.commit = c.hash;
    el.setAttribute('role', 'button');
    el.tabIndex = 0;
    el.innerHTML =
      svgGraph(rows[i], width) +
      `<div class="cbody"><div class="crow"><span class="chash">${esc(c.hash)}</span>` +
      `<span class="csubj">${esc(c.subject)}</span></div>` +
      `<span class="cdate">${esc(c.date)}</span></div>`;
    frag.appendChild(el);
  });
  list.innerHTML = '';
  list.appendChild(frag);
}

$('commitList').addEventListener('click', (e) => {
  const item = e.target.closest('.citem');
  if (!item) return;
  const hash = item.dataset.commit;
  state.mode = hash ? 'commit:' + hash : 'working';
  $('filesHeadLabel').textContent = hash ? hash : 'Files';
  $('filesHeadLabel').classList.toggle('hash', !!hash);
  $('sbMode').textContent = modeLabel();
  renderCommits();
  loadFiles();
});

function setView(afterOnly) {
  state.afterOnly = afterOnly;
  $('viewSbs').setAttribute('aria-pressed', String(!afterOnly));
  $('viewAfter').setAttribute('aria-pressed', String(afterOnly));
  renderDiff();
}
$('viewSbs').addEventListener('click', () => setView(false));
$('viewAfter').addEventListener('click', () => setView(true));

function updateStatus() {
  const i = state.files.findIndex((f) => f.path === state.current);
  $('sbFile').textContent =
    i >= 0 ? `${state.current} · ${i + 1}/${state.files.length}` : '';
}

/* ---------- toolbar ---------- */
function refreshAll() {
  if (!state.repo) return;
  if (state.historyMode) loadCommits();
  loadFiles(true);
}

/* ---------- watch: auto-refresh on repository changes ---------- */
let pendingRefresh = false;
window.__TAURI__.event.listen('repo-changed', () => {
  if (!state.repo) return;
  // don't yank the view while the user is writing feedback
  if (!$('popover').hidden) {
    pendingRefresh = true;
    return;
  }
  refreshAll();
});

function stepFile(delta) {
  if (!state.files.length) return;
  const i = state.files.findIndex((f) => f.path === state.current);
  const next = ((i < 0 ? 0 : i + delta) + state.files.length) % state.files.length;
  selectFile(state.files[next].path);
}

$('btnRefresh').addEventListener('click', refreshAll);
$('btnOpen').addEventListener('click', pickRepo);
$('emptyOpen').addEventListener('click', pickRepo);
$('fileFilter').addEventListener('input', (e) => {
  state.filter = e.target.value;
  renderTree();
});

/* ---------- window controls ---------- */
$('winMin').addEventListener('click', () => appWindow.minimize());
$('winMax').addEventListener('click', () => appWindow.toggleMaximize());
$('winClose').addEventListener('click', () => appWindow.close());

/* ---------- help overlay ---------- */
const KEYS = [
  { title: 'ナビゲーション', rows: [
    ['↑ / ↓', 'j / k', '行カーソル移動', true],
    ['F8 / Shift+F8', 'n / p', '次 / 前の変更ハンクへ', true],
    ['Ctrl+PgDn / Ctrl+PgUp', '', '次 / 前のファイルへ', false],
    ['Ctrl+F', '/', 'diff 内を検索', true],
    ['Home / End', 'g / G', 'ファイル先頭 / 末尾へ', false],
  ]},
  { title: '表示', rows: [
    ['Ctrl+1 / Ctrl+2', '', 'Side-by-side / After-only 切替', false],
    ['Ctrl+B', '', 'ファイルツリーの開閉', false],
    ['F5', '', '再 diff', false],
    ['クリック(折りたたみ行)', '', '変更なし区間の展開', false],
    ['ツールバーの検索欄', '', 'ファイル名の絞り込み', false],
  ]},
  { title: 'エージェントフィードバック', rows: [
    ['行番号クリック + Shift+クリック', '', '行範囲を選択', false],
    ['C', 'c', '選択範囲にコメントを書く', false],
    ['Ctrl+Enter', '', 'エージェントに送信(入力中)', false],
    ['Tab', '', 'コメント ⇄ 質問の種別切替(入力中)', false],
    ['R', '', '次の注釈スレッドへ', false],
  ]},
  { title: 'モード / アプリ', rows: [
    ['Alt+1 / 2 / 3', '', 'Working / Staged / History', false],
    ['Ctrl+O', '', 'リポジトリを開く', false],
    ['? または F1', '?', 'このショートカット一覧', false],
    ['Esc', '', '選択解除 / 閉じる', false],
  ]},
];

function renderKeys() {
  $('keyCards').innerHTML = KEYS.map((g) =>
    `<div class="keycard"><h3>${g.title}</h3><table>` +
    g.rows.map(([keys, alt, act, soon]) =>
      `<tr><td class="keys">` +
      keys.split(' / ').map((k) => `<kbd>${k}</kbd>`).join(' / ') +
      (alt ? `<span class="alt">vim: ${alt}</span>` : '') +
      `</td><td>${act}${soon ? ' <span class="badge soon">予定</span>' : ''}</td></tr>`
    ).join('') +
    `</table></div>`
  ).join('');
}
renderKeys();

const overlay = $('helpOverlay');
function openHelp() { overlay.classList.add('open'); }
function closeHelp() { overlay.classList.remove('open'); }
$('btnHelp').addEventListener('click', openHelp);
$('helpClose').addEventListener('click', closeHelp);
overlay.addEventListener('click', (e) => { if (e.target === overlay) closeHelp(); });

/* ---------- keyboard ---------- */
document.addEventListener('keydown', (e) => {
  const tag = e.target.tagName;
  if (tag === 'INPUT' || tag === 'TEXTAREA') {
    if (e.key === 'Escape') {
      if (e.target.id === 'popText') hidePopover();
      else e.target.blur();
    }
    return;
  }
  if (e.key === '?' || e.key === 'F1') {
    e.preventDefault();
    overlay.classList.contains('open') ? closeHelp() : openHelp();
  } else if (e.key === 'Escape') {
    if (!ctxMenu.hidden) hideCtxMenu();
    else if ($('editorOverlay').classList.contains('open')) closeEditorPicker();
    else if (overlay.classList.contains('open')) closeHelp();
    else if (!$('popover').hidden) hidePopover();
    else clearSelection();
  } else if (e.key === 'F5') {
    e.preventDefault();
    refreshAll();
  } else if (e.altKey && (e.key === '1' || e.key === '2' || e.key === '3')) {
    e.preventDefault();
    switchTab({ 1: 'working', 2: 'staged', 3: 'history' }[e.key]);
  } else if (e.ctrlKey && e.key === 'PageDown') {
    e.preventDefault();
    stepFile(1);
  } else if (e.ctrlKey && e.key === 'PageUp') {
    e.preventDefault();
    stepFile(-1);
  } else if (e.key === 'Home') {
    $('diffScroll').scrollTop = 0;
  } else if (e.key === 'End') {
    const s = $('diffScroll');
    s.scrollTop = s.scrollHeight;
  } else if (e.ctrlKey && (e.key === 'o' || e.key === 'O')) {
    e.preventDefault();
    pickRepo();
  } else if (e.ctrlKey && e.key === 'b') {
    e.preventDefault();
    $('treePane').classList.toggle('hidden');
  } else if (e.ctrlKey && e.key === '1') {
    e.preventDefault();
    setView(false);
  } else if (e.ctrlKey && e.key === '2') {
    e.preventDefault();
    setView(true);
  } else if ((e.key === 'c' || e.key === 'C') && !e.ctrlKey && !e.altKey && selRange()) {
    e.preventDefault();
    openPopover();
  } else if ((e.key === 'r' || e.key === 'R') && !e.ctrlKey && !e.altKey) {
    jumpNextThread();
  }
});

/* ---------- boot ---------- */
(async function init() {
  const gitVersion = await invoke('git_check');
  if (!gitVersion) {
    // the built-in gix backend covers reading; just let the user know
    toast('git コマンドが見つからないため、内蔵バックエンド (gix) で動作します');
  }
  const suggested = await invoke('initial_repo');
  if (suggested) await tryOpen(suggested, true);
  else showWelcome();
})();
