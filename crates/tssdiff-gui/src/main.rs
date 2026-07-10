#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use serde::Serialize;
use std::path::PathBuf;
use std::sync::Mutex;
use tauri::State;
use tssdiff_core::agent::{AgentSession, FeedbackKind};
use tssdiff_core::config::{Config, GitBackendKind};
use tssdiff_core::highlight;
use tssdiff_core::mode::OperationMode;
use tssdiff_core::parser::FileDiff;
use tssdiff_core::repo::{self, RepoBackend};
use tssdiff_core::side_by_side::{self, AlignedRow, RowKind};

/// Highlighting cap, mirroring the TUI's full-view cap: rows past this
/// line simply render unhighlighted
const HIGHLIGHT_CAP: usize = 4000;

#[derive(Default)]
struct AppState {
    /// Parsed diffs of the currently loaded file list; `load_diff`
    /// resolves file paths against this snapshot
    files: Mutex<Vec<FileDiff>>,
    /// Repo suggested at launch (CLI arg or the invocation directory)
    initial_path: Mutex<Option<String>>,
    /// Agent feedback session for the opened repo
    session: Mutex<Option<AgentSession>>,
    /// Core config (shared with the TUI: ~/.config/tssdiff/config.yaml)
    config: Mutex<Option<Config>>,
    /// Aligned rows of the file currently shown, for selections/anchors
    current_rows: Mutex<Vec<AlignedRow>>,
    current_file: Mutex<Option<String>>,
    /// Repository access, CLI or pure-Rust per config/auto-detection
    backend: Mutex<Option<RepoBackend>>,
}

#[derive(Serialize)]
struct RepoInfo {
    root: String,
    branch: String,
    /// Active backend name: "git" (CLI) or "gix" (built-in)
    backend: String,
}

#[derive(Serialize)]
struct FileEntry {
    path: String,
    added: usize,
    removed: usize,
}

#[derive(Serialize)]
struct Segment {
    /// CSS hex color, or null for the default text color
    c: Option<String>,
    t: String,
}

#[derive(Serialize)]
struct RowOut {
    kind: &'static str,
    old_no: Option<usize>,
    new_no: Option<usize>,
    old: Option<Vec<Segment>>,
    new: Option<Vec<Segment>>,
}

#[derive(Serialize)]
struct NoteOut {
    /// Index of the aligned row this note anchors to, if visible
    row: Option<usize>,
    author: String,
    body: String,
    old_line: Option<usize>,
    new_line: Option<usize>,
    /// Feedback id this note belongs to (own notes carry their payload
    /// id; agent replies reference the question they answer)
    reply_to: Option<String>,
}

#[derive(Serialize)]
struct DiffOut {
    rows: Vec<RowOut>,
    highlighted: bool,
    notes: Vec<NoteOut>,
    /// True when the file could not be decoded as text
    binary: bool,
}

#[derive(Serialize)]
struct CommitOut {
    hash: String,
    date: String,
    subject: String,
}

#[derive(Serialize)]
struct NotesOut {
    /// Notes anchored to the currently shown file
    notes: Vec<NoteOut>,
    /// Session-wide counts for the status bar
    sent: usize,
    replies: usize,
}

#[derive(Serialize)]
struct SendOut {
    /// Sink status line, e.g. "copied to clipboard"
    status: String,
    /// Payload id, used by the frontend to track awaited replies
    id: String,
    notes: Vec<NoteOut>,
    sent: usize,
    replies: usize,
}

fn mode_from(mode: &str) -> OperationMode {
    if let Some(commit) = mode.strip_prefix("commit:") {
        return OperationMode::GitCommit {
            commit: commit.to_string(),
        };
    }
    match mode {
        "staged" => OperationMode::GitCached,
        _ => OperationMode::GitWorkingDirectory,
    }
}

/// Absolute path of a repo-relative file (cwd is the repo root)
fn abs_repo_path(path: &str) -> Result<PathBuf, String> {
    let abs = std::env::current_dir()
        .map_err(|e| e.to_string())?
        .join(path);
    Ok(abs)
}

#[tauri::command]
fn copy_text(text: String) -> Result<(), String> {
    let mut clipboard = arboard::Clipboard::new().map_err(|e| e.to_string())?;
    clipboard.set_text(text).map_err(|e| e.to_string())
}

#[tauri::command]
fn open_in_editor(path: String, state: State<AppState>) -> Result<(), String> {
    let abs = abs_repo_path(&path)?;
    if !abs.exists() {
        return Err(format!("ファイルが存在しません: {path}"));
    }
    let configured = state
        .config
        .lock()
        .unwrap()
        .as_ref()
        .map(|c| c.editor.clone())
        .unwrap_or_default();
    let editor = if configured.trim().is_empty() {
        std::env::var("EDITOR").unwrap_or_default()
    } else {
        configured
    };
    if editor.trim().is_empty() {
        return open_with_default(&abs);
    }
    let mut parts = editor.split_whitespace();
    let program = parts.next().unwrap();
    std::process::Command::new(program)
        .args(parts)
        .arg(&abs)
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("エディタを起動できません ({program}): {e}"))
}

/// Open with the OS-associated application
fn open_with_default(path: &std::path::Path) -> Result<(), String> {
    #[cfg(windows)]
    {
        std::process::Command::new("cmd")
            .args(["/C", "start", ""])
            .arg(path)
            .spawn()
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
    #[cfg(not(windows))]
    {
        std::process::Command::new("xdg-open")
            .arg(path)
            .spawn()
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
}

#[tauri::command]
fn reveal_in_explorer(path: String) -> Result<(), String> {
    let abs = abs_repo_path(&path)?;
    #[cfg(windows)]
    {
        let select = format!("/select,{}", abs.display().to_string().replace('/', "\\"));
        std::process::Command::new("explorer")
            .arg(select)
            .spawn()
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
    #[cfg(not(windows))]
    {
        let parent = abs.parent().unwrap_or(&abs).to_path_buf();
        std::process::Command::new("xdg-open")
            .arg(parent)
            .spawn()
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
}

/// TSSDIFF_GIT_BACKEND=cli|pure|auto overrides the configured backend
fn backend_kind_override() -> Option<GitBackendKind> {
    match std::env::var("TSSDIFF_GIT_BACKEND")
        .ok()?
        .to_lowercase()
        .as_str()
    {
        "cli" => Some(GitBackendKind::Cli),
        "pure" | "gix" => Some(GitBackendKind::Pure),
        "auto" => Some(GitBackendKind::Auto),
        _ => None,
    }
}

#[tauri::command]
fn initial_repo(state: State<AppState>) -> Option<String> {
    state.initial_path.lock().unwrap().clone()
}

/// Version string when git is on PATH, None otherwise - the frontend
/// notes that the built-in backend will cover for it
#[tauri::command]
fn git_check() -> Option<String> {
    repo::git_cli_version()
}

#[tauri::command]
fn load_commits(state: State<AppState>) -> Result<Vec<CommitOut>, String> {
    let backend = state.backend.lock().unwrap();
    let backend = backend.as_ref().ok_or("リポジトリが開かれていません")?;
    backend
        .commit_log(300)
        .map_err(|e| e.to_string())
        .map(|commits| {
            commits
                .into_iter()
                .map(|c| CommitOut {
                    hash: c.hash,
                    date: c.date,
                    subject: c.subject,
                })
                .collect()
        })
}

#[tauri::command]
fn open_repo(path: String, state: State<AppState>) -> Result<RepoInfo, String> {
    let dir = PathBuf::from(&path);
    std::env::set_current_dir(&dir).map_err(|e| format!("フォルダを開けません: {e}"))?;

    let config = Config::load().map_err(|e| e.to_string())?;
    let kind = backend_kind_override().unwrap_or(config.git.backend);
    let backend = RepoBackend::open(&dir, kind).map_err(|e| e.to_string())?;
    let root = backend.toplevel().map_err(|e| e.to_string())?;
    std::env::set_current_dir(&root).map_err(|e| e.to_string())?;
    let branch = backend.branch().unwrap_or_else(|| "?".to_string());
    let backend_name = backend.name().to_string();

    *state.session.lock().unwrap() = Some(AgentSession::new(root.clone()));
    *state.config.lock().unwrap() = Some(config);
    *state.backend.lock().unwrap() = Some(backend);
    *state.current_file.lock().unwrap() = None;
    state.current_rows.lock().unwrap().clear();

    Ok(RepoInfo {
        root: root.display().to_string(),
        branch,
        backend: backend_name,
    })
}

#[tauri::command]
fn load_files(mode: String, state: State<AppState>) -> Result<Vec<FileEntry>, String> {
    let op = mode_from(&mode);
    let files = {
        let backend = state.backend.lock().unwrap();
        let backend = backend.as_ref().ok_or("リポジトリが開かれていません")?;
        backend.changed_files(&op).map_err(|e| e.to_string())?
    };
    let entries = files
        .iter()
        .map(|f| FileEntry {
            path: f.filename.clone(),
            added: f.added_lines,
            removed: f.removed_lines,
        })
        .collect();
    *state.files.lock().unwrap() = files;
    Ok(entries)
}

#[tauri::command]
fn load_diff(
    mode: String,
    path: String,
    theme: String,
    state: State<AppState>,
) -> Result<DiffOut, String> {
    let op = mode_from(&mode);
    let file = state
        .files
        .lock()
        .unwrap()
        .iter()
        .find(|f| f.filename == path)
        .cloned()
        .ok_or_else(|| format!("ファイルが見つかりません: {path}"))?;

    let versions = {
        let backend = state.backend.lock().unwrap();
        let backend = backend.as_ref().ok_or("リポジトリが開かれていません")?;
        backend.file_versions(&op, &file)
    };
    let (old_text, new_text) = match versions {
        Ok(pair) => pair,
        // Undecodable content means a binary file, not a failure
        Err(e) if e.to_string().to_lowercase().contains("utf-8") => {
            return Ok(DiffOut {
                rows: Vec::new(),
                highlighted: false,
                notes: Vec::new(),
                binary: true,
            });
        }
        Err(e) => return Err(e.to_string()),
    };

    let rows = side_by_side::align(&old_text, &new_text);
    let hl = highlight::highlight_pair(
        &file.filename,
        &old_text,
        &new_text,
        &theme,
        Some(HIGHLIGHT_CAP),
    );
    let highlighted = hl.is_some();
    let (old_hl, new_hl) = match hl {
        Some((o, n)) => (Some(o), Some(n)),
        None => (None, None),
    };

    let rows_out = rows
        .iter()
        .map(|row| {
            let kind = match row.kind {
                RowKind::Context => "ctx",
                RowKind::Removed => "del",
                RowKind::Added => "add",
                RowKind::Modified => "mod",
            };
            RowOut {
                kind,
                old_no: row.old.as_ref().map(|(n, _)| *n),
                new_no: row.new.as_ref().map(|(n, _)| *n),
                old: row
                    .old
                    .as_ref()
                    .map(|(n, t)| segments(old_hl.as_ref(), *n, t)),
                new: row
                    .new
                    .as_ref()
                    .map(|(n, t)| segments(new_hl.as_ref(), *n, t)),
            }
        })
        .collect();

    let notes = anchored_notes(&state, &path, &rows);
    *state.current_rows.lock().unwrap() = rows;
    *state.current_file.lock().unwrap() = Some(path);

    Ok(DiffOut {
        rows: rows_out,
        highlighted,
        notes,
        binary: false,
    })
}

/// Notes of `file` mapped onto row indices of the given alignment
fn map_notes(session: &AgentSession, file: &str, rows: &[AlignedRow]) -> Vec<NoteOut> {
    let file_norm = file.replace('\\', "/");
    session
        .notes
        .iter()
        .filter(|n| n.file == file_norm)
        .map(|n| NoteOut {
            row: rows.iter().position(|row| n.anchors_to(row)),
            author: n.author.clone(),
            body: n.body.clone(),
            old_line: n.old_line,
            new_line: n.new_line,
            reply_to: n.reply_to.clone(),
        })
        .collect()
}

fn anchored_notes(state: &State<AppState>, file: &str, rows: &[AlignedRow]) -> Vec<NoteOut> {
    let session = state.session.lock().unwrap();
    match session.as_ref() {
        Some(session) => map_notes(session, file, rows),
        None => Vec::new(),
    }
}

fn note_counts(session: &AgentSession) -> (usize, usize) {
    let sent = session.notes.iter().filter(|n| n.author == "you").count();
    (sent, session.notes.len() - sent)
}

#[tauri::command]
fn send_feedback(
    kind: String,
    comment: String,
    sel_start: usize,
    sel_end: usize,
    state: State<AppState>,
) -> Result<SendOut, String> {
    let file = state
        .current_file
        .lock()
        .unwrap()
        .clone()
        .ok_or("ファイルが選択されていません")?;
    let rows = state.current_rows.lock().unwrap().clone();
    if rows.is_empty() {
        return Err("diff が読み込まれていません".to_string());
    }
    let config = state
        .config
        .lock()
        .unwrap()
        .clone()
        .ok_or("設定が読み込まれていません")?;

    let kind = if kind == "question" {
        FeedbackKind::Question
    } else {
        FeedbackKind::Comment
    };

    let mut session_guard = state.session.lock().unwrap();
    let session = session_guard.as_mut().ok_or("セッションがありません")?;
    let payload = session.build_payload(kind, &file, &rows, sel_start, sel_end, &comment);
    let status = session
        .send(&config.agent, &payload)
        .map_err(|e| e.to_string())?;

    let notes = map_notes(session, &file, &rows);
    let (sent, replies) = note_counts(session);

    Ok(SendOut {
        status,
        id: payload.id,
        notes,
        sent,
        replies,
    })
}

#[tauri::command]
fn poll_notes(state: State<AppState>) -> Result<NotesOut, String> {
    let mut session_guard = state.session.lock().unwrap();
    let Some(session) = session_guard.as_mut() else {
        return Ok(NotesOut {
            notes: Vec::new(),
            sent: 0,
            replies: 0,
        });
    };
    session.poll_replies();
    let (sent, replies) = note_counts(session);

    let file = state.current_file.lock().unwrap().clone();
    let rows = state.current_rows.lock().unwrap();
    let notes = match file {
        Some(file) => map_notes(session, &file, &rows),
        None => Vec::new(),
    };

    Ok(NotesOut {
        notes,
        sent,
        replies,
    })
}

/// Segments of one display line: syntax colors when the highlighter
/// covered this line, otherwise the raw text uncolored
fn segments(hl: Option<&highlight::HighlightedLines>, lineno: usize, raw: &str) -> Vec<Segment> {
    if let Some(line) = hl.and_then(|lines| lines.get(lineno - 1)) {
        return line
            .iter()
            .map(|(color, text)| Segment {
                c: color.map(|(r, g, b)| format!("#{r:02x}{g:02x}{b:02x}")),
                t: text.clone(),
            })
            .collect();
    }
    vec![Segment {
        c: None,
        t: raw.to_string(),
    }]
}

fn main() {
    // `tssdiff-gui [path]` opens that repo; a bare launch from a
    // terminal offers the invocation directory
    let initial = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok())
        .and_then(|p| std::fs::canonicalize(p).ok())
        .map(|p| p.display().to_string());

    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(AppState {
            initial_path: Mutex::new(initial),
            ..Default::default()
        })
        .invoke_handler(tauri::generate_handler![
            initial_repo,
            git_check,
            open_repo,
            load_files,
            load_commits,
            load_diff,
            send_feedback,
            poll_notes,
            copy_text,
            open_in_editor,
            reveal_in_explorer
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
