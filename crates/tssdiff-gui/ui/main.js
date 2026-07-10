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
  let hasChange = false;
  rows.forEach((r, i) => {
    if (r.kind !== 'ctx') {
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

function renderDiff() {
  const frag = document.createDocumentFragment();
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
  }
  diffBody.innerHTML = '';
  diffBody.appendChild(frag);
  diffBody.parentElement.classList.toggle('after-only', state.afterOnly);
  applySelection();
}

diffBody.addEventListener('click', (e) => {
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
}

function clearSelection() {
  state.sel.start = state.sel.end = null;
  applySelection();
}

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
    ['C', 'c', '選択範囲にコメントを書く', true],
    ['Ctrl+Enter', '', 'エージェントに送信', true],
    ['R', '', '次の返信スレッドへ', true],
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
    if (e.key === 'Escape') e.target.blur();
    return;
  }
  if (e.key === '?' || e.key === 'F1') {
    e.preventDefault();
    overlay.classList.contains('open') ? closeHelp() : openHelp();
  } else if (e.key === 'Escape') {
    if (overlay.classList.contains('open')) closeHelp();
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
    toast('エージェントフィードバックは Phase 2 で実装予定です');
  }
});

/* ---------- boot ---------- */
(async function init() {
  const suggested = await invoke('initial_repo');
  if (suggested) await tryOpen(suggested, true);
  else showWelcome();
})();
