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
  mode: 'working',   // 'working' | 'staged'
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
    const name = info.root.split(/[\\/]/).pop();
    appWindow.setTitle(name + ' — tssdiff');
    await loadFiles();
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
  $('emptyBody').textContent =
    state.mode === 'staged' ? 'ステージされた変更がありません。' : 'ワーキングツリーはクリーンです。';
  $('emptyOpen').hidden = true;
  $('fileHead').hidden = true;
  diffBody.innerHTML = '';
}

/* ---------- file list / tree ---------- */
async function loadFiles() {
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

async function loadDiff(path) {
  let out;
  try {
    out = await invoke('load_diff', { mode: state.mode, path, theme: syntectTheme() });
  } catch (e) {
    toast(String(e));
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
  $('diffScroll').scrollTop = 0;
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
$('modeTabs').addEventListener('click', (e) => {
  const btn = e.target.closest('button[data-mode]');
  if (!btn) return;
  const mode = btn.dataset.mode;
  if (mode === 'history') {
    toast('History モードは今後のフェーズで実装予定です');
    return;
  }
  state.mode = mode;
  document.querySelectorAll('#modeTabs button').forEach((b) =>
    b.setAttribute('aria-selected', String(b === btn))
  );
  $('sbMode').textContent = mode === 'staged' ? 'Staged' : 'Working Tree';
  if (state.repo) loadFiles();
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
$('btnRefresh').addEventListener('click', () => state.repo && loadFiles());
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
    ['Ctrl+PgDn / Ctrl+PgUp', '', '次 / 前のファイルへ', true],
    ['Ctrl+F', '/', 'diff 内を検索', true],
    ['Home / End', 'g / G', 'ファイル先頭 / 末尾へ', true],
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
    ['Alt+1 / 2 / 3', '', 'Working / Staged / History', true],
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
    if (overlay.classList.contains('open')) closeHelp();
    else if (!$('popover').hidden) hidePopover();
    else clearSelection();
  } else if (e.key === 'F5') {
    e.preventDefault();
    if (state.repo) loadFiles();
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
  const suggested = await invoke('initial_repo');
  if (suggested) await tryOpen(suggested, true);
  else showWelcome();
})();
