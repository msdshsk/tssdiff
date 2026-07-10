#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use serde::Serialize;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Mutex;
use tauri::State;
use tssdiff_core::git::GitExecutor;
use tssdiff_core::highlight;
use tssdiff_core::mode::OperationMode;
use tssdiff_core::parser::{DiffParser, FileDiff};
use tssdiff_core::side_by_side::{self, RowKind};

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
}

#[derive(Serialize)]
struct RepoInfo {
    root: String,
    branch: String,
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
struct DiffOut {
    rows: Vec<RowOut>,
    highlighted: bool,
}

fn mode_from(mode: &str) -> OperationMode {
    match mode {
        "staged" => OperationMode::GitCached,
        _ => OperationMode::GitWorkingDirectory,
    }
}

fn git_branch() -> Option<String> {
    let out = Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

#[tauri::command]
fn initial_repo(state: State<AppState>) -> Option<String> {
    state.initial_path.lock().unwrap().clone()
}

#[tauri::command]
fn open_repo(path: String) -> Result<RepoInfo, String> {
    let dir = PathBuf::from(&path);
    std::env::set_current_dir(&dir).map_err(|e| format!("フォルダを開けません: {e}"))?;
    if !GitExecutor::is_git_repo() {
        return Err(format!("git リポジトリではありません: {path}"));
    }
    let root = GitExecutor::toplevel().map_err(|e| e.to_string())?;
    std::env::set_current_dir(&root).map_err(|e| e.to_string())?;
    let branch = git_branch().unwrap_or_else(|| "?".to_string());
    Ok(RepoInfo {
        root: root.display().to_string(),
        branch,
    })
}

#[tauri::command]
fn load_files(mode: String, state: State<AppState>) -> Result<Vec<FileEntry>, String> {
    let op = mode_from(&mode);
    let diff = GitExecutor::new().get_diff(&op).map_err(|e| e.to_string())?;
    let files = DiffParser::parse(&diff);
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

    let (old_text, new_text) = GitExecutor::new()
        .get_file_versions(&op, &file)
        .map_err(|e| e.to_string())?;

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

    let rows = rows
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
                old: row.old.as_ref().map(|(n, t)| segments(old_hl.as_ref(), *n, t)),
                new: row.new.as_ref().map(|(n, t)| segments(new_hl.as_ref(), *n, t)),
            }
        })
        .collect();

    Ok(DiffOut { rows, highlighted })
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
            files: Mutex::new(Vec::new()),
            initial_path: Mutex::new(initial),
        })
        .invoke_handler(tauri::generate_handler![
            initial_repo,
            open_repo,
            load_files,
            load_diff
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
