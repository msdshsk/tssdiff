mod cli;
mod render;

use crate::cli::{Cli, Commands};
use crate::render::{
    render_comment_input, render_commit_graph, render_commit_input, render_commit_list,
    render_diff_content, render_file_list, render_help_overlay, render_menu_bar, render_search_box,
    render_side_by_side, render_status_line, render_warning_bar,
};
use anyhow::Result;
use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers,
        MouseButton, MouseEvent, MouseEventKind,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Position, Rect},
    widgets::ListState,
};
use std::io::{self, Read};
use std::process::{Command, Stdio};
use tssdiff_core::agent::{self, AgentSession, FeedbackKind};
use tssdiff_core::config::{self, Config, DiffCommandType};
use tssdiff_core::git::{CommitInfo, GitExecutor, GraphRow};
use tssdiff_core::mode::OperationMode;
use tssdiff_core::parser::{DiffFileKey, DiffParser, FileDiff};
use tssdiff_core::persistence::PersistenceManager;
use tssdiff_core::side_by_side::{self, AlignedRow};
use tssdiff_core::theme::Theme;
use tssdiff_core::tree::{FileTreeBuilder, FileTreeItem};
use tssdiff_core::{highlight, icons};

/// Syntax colors per side, mapped to ratatui for rendering
type HighlightedLines = Vec<Vec<(ratatui::style::Color, String)>>;

/// Map core highlight segments (sRGB or default) onto ratatui colors
fn to_tui_highlight(lines: highlight::HighlightedLines) -> HighlightedLines {
    lines
        .into_iter()
        .map(|line| {
            line.into_iter()
                .map(|(color, text)| {
                    let color = match color {
                        Some((r, g, b)) => ratatui::style::Color::Rgb(r, g, b),
                        None => ratatui::style::Color::Reset,
                    };
                    (color, text)
                })
                .collect()
        })
        .collect()
}

// Constants for external tool integration
const DEFAULT_TERMINAL_HEIGHT: &str = "50";
const DEFAULT_TERMINAL_TYPE: &str = "xterm-256color";

/// Context lines kept around each change in the condensed view
const CONDENSED_CONTEXT: usize = 3;
/// Full-view highlighting stops after this many lines to keep
/// file-to-file navigation responsive on huge files
const HIGHLIGHT_FULL_VIEW_CAP: usize = 4000;

/// How the diff pane presents a file's changes
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ViewMode {
    /// Before/after panes rendered in-app (default)
    SideBySide,
    /// Only the After pane, full width with syntax highlighting -
    /// reading the changed file rather than comparing
    AfterOnly,
    /// Raw unified diff, optionally piped through an external tool
    Unified,
}

/// What the left pane currently lists
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LeftPane {
    Files,
    History,
}

/// Clickable menu bar entries
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MenuAction {
    Files,
    History,
    ToggleView,
    Help,
    Quit,
}

/// Screen regions captured during rendering, used for mouse hit-testing
#[derive(Default, Clone)]
struct UiRegions {
    menu_items: Vec<(Rect, MenuAction)>,
    left_column: Rect,
    list_area: Rect,
    graph_area: Rect,
}

// Template variable values for command substitution
#[derive(Debug, Clone)]
struct TemplateValues {
    width: u16,
    column_width: u16,
    diff_area_width: u16,
    diff_column_width: u16,
}

struct App {
    should_quit: bool,
    config: Config,
    theme: Theme,
    diff_output: String,
    file_tree_items: Vec<FileTreeItem>,
    original_file_diffs: Vec<FileDiff>, // Store original file diffs
    selected_index: usize,
    vertical_scroll: u16,
    horizontal_scroll: u16,
    collapsed_directories: std::collections::HashSet<String>, // Track collapsed directories
    checked_files: std::collections::HashSet<String>,         // Track checked files by path
    persistence_manager: PersistenceManager,                  // For saving/loading check states
    git_executor: Option<GitExecutor>,                        // For getting individual file diffs
    operation_mode: OperationMode,                            // Track how the app was invoked
    // Search functionality
    search_mode: bool,                           // Track if we're in search mode
    search_input_mode: bool,                     // Track if we're actively typing in search
    search_query: String,                        // Current search query
    filtered_file_tree_items: Vec<FileTreeItem>, // Filtered items for search
    // UI state
    file_list_state: ListState,      // For stateful file tree scrolling
    warning_message: Option<String>, // Warning to display in the warning bar below the diff pane
    view_mode: ViewMode,
    aligned_rows: Option<Vec<AlignedRow>>, // Before/after rows for the current file
    display_rows: Vec<side_by_side::DisplayRow>, // Condensed (or full) row order to draw
    condensed: bool,                       // Hunks-only view vs full file
    highlighted: Option<(HighlightedLines, HighlightedLines)>, // Syntax colors per side
    // Commit history browsing
    left_pane: LeftPane,
    commits: Vec<CommitInfo>,     // Loaded lazily when History opens
    commit_index: usize,          // 0 = virtual working-tree entry, 1.. = commits
    commit_list_state: ListState, // For stateful commit list scrolling
    graph_rows: Vec<GraphRow>,    // git log --graph rows for the graph pane
    graph_scroll: usize,          // Scroll offset used by the last graph render
    show_help: bool,              // Help overlay visibility
    regions: UiRegions,           // Mouse hit-test regions from the last frame
    // Staging and live refresh
    staged_files: std::collections::HashSet<String>, // Repo-root-relative staged paths
    commit_input_mode: bool,                         // Commit message box visibility
    commit_message: String,                          // Commit message being typed
    last_refresh_check: std::time::Instant,          // Auto-refresh timer
    last_worktree_diff: Option<String>,              // Raw diff snapshot for change detection
    // Agent feedback (c key): line cursor, input box, and reply notes
    agent_session: AgentSession,
    comment_cursor: Option<usize>, // Display-row index; Some = comment mode active
    comment_anchor: Option<usize>, // v key: range selection anchor (display-row index)
    comment_input_mode: bool,      // Comment text box visibility
    comment_text: String,          // Comment being typed
    comment_kind: FeedbackKind,    // Comment vs Question (Tab toggles)
    last_diff_height: u16,         // Diff pane rows from the last frame, for cursor scrolling
    file_pane_hidden: bool,        // z key: hide the left pane, diff takes the full width
    wrapped_notes: Vec<Vec<String>>, // Notes wrapped (and folded) to the note pane width
    notes_expanded: bool,          // n key: show long notes in full
    last_note_wrap_width: u16,     // Note pane content width from the last frame
}

impl App {
    fn new(
        config: Config,
        file_diffs: Vec<FileDiff>,
        operation_mode: OperationMode,
    ) -> Result<Self> {
        let file_tree_items = if config.flat_file_list {
            FileTreeBuilder::build_flat_list(&file_diffs)
        } else {
            FileTreeBuilder::build_file_tree(&file_diffs)
        };
        let theme = config.theme.clone();

        let diff_output = if file_tree_items.is_empty() {
            String::from("No diff content available")
        } else if file_tree_items[0].is_directory {
            format!("Directory: {}", file_tree_items[0].full_path)
        } else if let Some(ref file_diff) = file_tree_items[0].file_diff {
            file_diff.content.clone()
        } else {
            String::from("No diff content available")
        };

        // Initialize persistence manager
        let persistence_manager = PersistenceManager::new()?;

        // Initialize git executor if needed for interactive file viewing
        let git_executor = if operation_mode.requires_git_repo() {
            Some(GitExecutor::new())
        } else {
            None
        };

        let config_condensed = config.condensed_view;

        // Load existing check states
        let diff_keys: Vec<DiffFileKey> = file_diffs
            .iter()
            .filter_map(|fd| fd.diff_key.clone())
            .collect();

        let checked_files = persistence_manager
            .load_checked_files(&diff_keys)
            .unwrap_or_else(|_| std::collections::HashSet::new());

        let mut app = Self {
            should_quit: false,
            config,
            theme,
            diff_output,
            file_tree_items: file_tree_items.clone(),
            original_file_diffs: file_diffs,
            selected_index: 0,
            vertical_scroll: 0,
            horizontal_scroll: 0,
            collapsed_directories: std::collections::HashSet::new(),
            checked_files,
            persistence_manager,
            git_executor,
            operation_mode,
            search_mode: false,
            search_input_mode: false,
            search_query: String::new(),
            filtered_file_tree_items: file_tree_items,
            file_list_state: {
                let mut state = ListState::default();
                state.select(Some(0));
                state
            },
            warning_message: None,
            view_mode: ViewMode::SideBySide,
            aligned_rows: None,
            display_rows: Vec::new(),
            condensed: config_condensed,
            highlighted: None,
            left_pane: LeftPane::Files,
            commits: Vec::new(),
            commit_index: 0,
            commit_list_state: {
                let mut state = ListState::default();
                state.select(Some(0));
                state
            },
            graph_rows: Vec::new(),
            graph_scroll: 0,
            show_help: false,
            regions: UiRegions::default(),
            staged_files: std::collections::HashSet::new(),
            commit_input_mode: false,
            commit_message: String::new(),
            last_refresh_check: std::time::Instant::now(),
            last_worktree_diff: None,
            agent_session: AgentSession::new(
                GitExecutor::toplevel()
                    .or_else(|_| std::env::current_dir())
                    .unwrap_or_default(),
            ),
            comment_cursor: None,
            comment_anchor: None,
            comment_input_mode: false,
            comment_text: String::new(),
            comment_kind: FeedbackKind::Comment,
            last_diff_height: 30,
            file_pane_hidden: false,
            wrapped_notes: Vec::new(),
            notes_expanded: false,
            last_note_wrap_width: 0,
        };
        app.refresh_aligned_rows();
        app.refresh_staged_files();
        Ok(app)
    }

    fn is_working_tree_mode(&self) -> bool {
        matches!(
            self.operation_mode,
            OperationMode::GitWorkingDirectory | OperationMode::GitStatus
        )
    }

    fn refresh_staged_files(&mut self) {
        self.staged_files = if self.is_working_tree_mode() {
            GitExecutor::new().staged_files().unwrap_or_default()
        } else {
            std::collections::HashSet::new()
        };
    }

    /// Checked files that exist in the current list, for staging operations
    fn checked_paths(&self) -> Vec<String> {
        self.file_tree_items
            .iter()
            .filter(|item| !item.is_directory && self.checked_files.contains(&item.full_path))
            .map(|item| item.full_path.clone())
            .collect()
    }

    /// Stage (s) or unstage (S) every checked file
    fn stage_checked_files(&mut self, stage: bool) {
        if !self.is_working_tree_mode() {
            self.warning_message = Some("Staging works in the working tree view".to_string());
            return;
        }
        let paths = self.checked_paths();
        if paths.is_empty() {
            self.warning_message = Some("No checked files - mark them with Tab first".to_string());
            return;
        }

        let executor = GitExecutor::new();
        let result = if stage {
            executor.stage_files(&paths)
        } else {
            executor.unstage_files(&paths)
        };
        match result {
            Ok(()) => {
                let action = if stage { "Staged" } else { "Unstaged" };
                self.warning_message = Some(format!("{} {} file(s)", action, paths.len()));
                self.refresh_staged_files();
            }
            Err(e) => self.warning_message = Some(format!("{e}")),
        }
    }

    fn begin_commit(&mut self) {
        if !self.is_working_tree_mode() {
            self.warning_message = Some("Committing works in the working tree view".to_string());
            return;
        }
        self.refresh_staged_files();
        if self.staged_files.is_empty() {
            self.warning_message =
                Some("Nothing staged - check files (Tab) and stage them (s) first".to_string());
            return;
        }
        self.commit_input_mode = true;
        self.commit_message.clear();
    }

    fn execute_commit(&mut self) {
        let message = self.commit_message.trim().to_string();
        if message.is_empty() {
            self.warning_message = Some("Commit message is empty".to_string());
            return;
        }
        self.commit_input_mode = false;
        self.commit_message.clear();

        match GitExecutor::new().commit(&message) {
            Ok(_) => {
                self.warning_message = Some(format!("Committed: {message}"));
                // History caches are stale after a commit
                self.commits.clear();
                self.graph_rows.clear();
                self.reload_diffs();
            }
            Err(e) => self.warning_message = Some(format!("{e}")),
        }
    }

    /// Re-fetch diffs for the current mode, keeping the selection if possible
    fn reload_diffs(&mut self) {
        match get_diffs_from_git(&self.operation_mode) {
            Ok(diffs) => {
                // Auto-refresh re-snapshots on its next tick
                self.last_worktree_diff = None;
                self.apply_reloaded_diffs(diffs);
            }
            Err(e) => self.warning_message = Some(format!("Reload failed: {e}")),
        }
    }

    fn apply_reloaded_diffs(&mut self, diffs: Vec<FileDiff>) {
        let selected_path = self
            .get_current_file_tree_items()
            .get(self.selected_index)
            .map(|item| item.full_path.clone());

        let diff_keys: Vec<DiffFileKey> =
            diffs.iter().filter_map(|fd| fd.diff_key.clone()).collect();
        self.checked_files = self
            .persistence_manager
            .load_checked_files(&diff_keys)
            .unwrap_or_default();

        self.original_file_diffs = diffs;
        self.rebuild_file_tree();
        if self.search_mode {
            self.update_search_filter();
        } else {
            self.filtered_file_tree_items = self.file_tree_items.clone();
        }

        // Restore the previous selection if the file still exists
        let items = self.get_current_file_tree_items();
        let restored = selected_path
            .and_then(|path| items.iter().position(|item| item.full_path == path))
            .unwrap_or_else(|| self.selected_index.min(items.len().saturating_sub(1)));
        self.selected_index = restored;
        self.file_list_state.select(Some(self.selected_index));

        if self.file_tree_items.is_empty() {
            self.diff_output = String::from("No diff content available");
            self.aligned_rows = None;
        }
        self.refresh_staged_files();
        self.update_diff_content();
    }

    /// Poll the working tree every couple of seconds and reload when the
    /// diff actually changed, so reviews track an AI editing files live
    fn auto_refresh_if_due(&mut self) {
        if !self.is_working_tree_mode()
            || self.left_pane != LeftPane::Files
            || self.search_input_mode
            || self.commit_input_mode
            || self.comment_cursor.is_some()
            || self.show_help
        {
            return;
        }
        if self.last_refresh_check.elapsed() < std::time::Duration::from_secs(2) {
            return;
        }
        self.last_refresh_check = std::time::Instant::now();

        let Ok(current) = GitExecutor::new().get_diff(&self.operation_mode) else {
            return;
        };
        match &self.last_worktree_diff {
            None => self.last_worktree_diff = Some(current),
            Some(previous) if *previous != current => {
                let diffs = DiffParser::parse(&current);
                self.last_worktree_diff = Some(current);
                self.apply_reloaded_diffs(diffs);
            }
            _ => {}
        }
    }

    /// Total entries in the history list (virtual working-tree entry + commits)
    fn history_len(&self) -> usize {
        1 + self.commits.len()
    }

    /// Switch the left pane to the commit history, loading it on first use
    fn open_history(&mut self) {
        if !GitExecutor::is_git_repo() {
            self.warning_message = Some("History requires a git repository".to_string());
            return;
        }
        if self.search_mode {
            self.exit_search_mode();
        }
        if self.commits.is_empty() {
            match GitExecutor::new().get_commit_log(300) {
                Ok(commits) => self.commits = commits,
                Err(e) => {
                    self.warning_message = Some(format!("Failed to load history: {e}"));
                }
            }
        }
        if self.graph_rows.is_empty() {
            self.graph_rows = GitExecutor::new().get_commit_graph(300).unwrap_or_default();
        }
        self.left_pane = LeftPane::History;
        self.preview_history_entry();
    }

    /// Graph row belonging to the currently selected commit
    fn graph_selected_row(&self) -> Option<usize> {
        if self.commit_index == 0 {
            return None;
        }
        let hash = &self.commits.get(self.commit_index - 1)?.hash;
        self.graph_rows
            .iter()
            .position(|row| row.hash.as_deref() == Some(hash.as_str()))
    }

    fn show_files_pane(&mut self) {
        self.left_pane = LeftPane::Files;
        self.update_diff_content();
    }

    /// Move the history selection and preview the entry in the diff pane
    fn history_move(&mut self, delta: isize) {
        let len = self.history_len();
        let new_index = self
            .commit_index
            .saturating_add_signed(delta)
            .min(len.saturating_sub(1));
        self.history_select(new_index);
    }

    fn history_select(&mut self, index: usize) {
        self.commit_index = index.min(self.history_len().saturating_sub(1));
        self.commit_list_state.select(Some(self.commit_index));
        self.preview_history_entry();
    }

    /// Show commit metadata + diffstat (or working-tree status) on the right
    fn preview_history_entry(&mut self) {
        let executor = GitExecutor::new();
        let preview = if self.commit_index == 0 {
            let bullet = icons::bullet(self.config.icon_mode);
            executor.get_status_summary().map(|status| {
                format!("{bullet} Working tree changes\n\n{status}\nEnter/click: open")
            })
        } else {
            match self.commits.get(self.commit_index - 1) {
                Some(commit) => executor
                    .get_commit_summary(&commit.hash)
                    .map(|summary| format!("{summary}\nEnter/click: open this commit")),
                None => return,
            }
        };

        match preview {
            Ok(text) => self.diff_output = text,
            Err(e) => self.diff_output = format!("Failed to load preview: {e}"),
        }
        self.aligned_rows = None;
        self.vertical_scroll = 0;
        self.horizontal_scroll = 0;
    }

    /// Load the selected history entry's changes into the Files pane
    fn open_selected_history_entry(&mut self) {
        let mode = if self.commit_index == 0 {
            OperationMode::GitWorkingDirectory
        } else {
            match self.commits.get(self.commit_index - 1) {
                Some(commit) => OperationMode::GitCommit {
                    commit: commit.hash.clone(),
                },
                None => return,
            }
        };

        match get_diffs_from_git(&mode) {
            Ok(diffs) if diffs.is_empty() => {
                self.warning_message = Some("No changes in this entry".to_string());
            }
            Ok(diffs) => {
                let diff_keys: Vec<DiffFileKey> =
                    diffs.iter().filter_map(|fd| fd.diff_key.clone()).collect();
                self.checked_files = self
                    .persistence_manager
                    .load_checked_files(&diff_keys)
                    .unwrap_or_default();

                self.operation_mode = mode;
                self.git_executor = Some(GitExecutor::new());
                self.original_file_diffs = diffs;
                self.collapsed_directories.clear();
                self.rebuild_file_tree();
                self.filtered_file_tree_items = self.file_tree_items.clone();
                self.selected_index = 0;
                self.file_list_state.select(Some(0));
                self.left_pane = LeftPane::Files;
                self.warning_message = None;
                self.update_diff_content();
            }
            Err(e) => {
                self.warning_message = Some(format!("Failed to load changes: {e}"));
            }
        }
    }

    fn toggle_view_mode(&mut self) {
        self.view_mode = match self.view_mode {
            ViewMode::SideBySide => ViewMode::AfterOnly,
            ViewMode::AfterOnly => ViewMode::Unified,
            ViewMode::Unified => ViewMode::SideBySide,
        };
        self.update_diff_content();
    }

    /// Shift+Left/Right moves the pane divider: positive delta widens
    /// the file pane. Resizing a hidden pane brings it back first
    fn resize_file_pane(&mut self, delta: i16) {
        if self.file_pane_hidden {
            self.file_pane_hidden = false;
            return;
        }
        let percent = (self.config.file_pane_percent as i16 + delta).clamp(10, 60);
        self.config.file_pane_percent = percent as u16;
    }

    fn toggle_file_pane(&mut self) {
        self.file_pane_hidden = !self.file_pane_hidden;
    }

    /// Recompute the before/after rows for the currently selected file
    fn refresh_aligned_rows(&mut self) {
        self.aligned_rows = None;
        self.display_rows.clear();
        self.highlighted = None;
        if self.view_mode == ViewMode::Unified {
            return;
        }

        let Some(file_diff) = self
            .get_current_file_tree_items()
            .get(self.selected_index)
            .and_then(|item| item.file_diff.clone())
        else {
            return;
        };

        // GitExecutor is stateless, and file/directory compare modes work
        // without a repository, so a fresh instance always suffices
        let executor = GitExecutor::new();
        if let Ok((old_text, new_text)) =
            executor.get_file_versions(&self.operation_mode, &file_diff)
        {
            let rows = side_by_side::align(&old_text, &new_text);
            self.rebuild_wrapped_notes(&rows);
            self.display_rows = self.build_display_rows(&rows);

            if self.config.syntax_highlight {
                // Highlighting must run from the file start, so cap it at
                // the last line the view can actually show
                let cap = if self.condensed {
                    side_by_side::max_needed_line(&rows, &self.display_rows)
                } else {
                    HIGHLIGHT_FULL_VIEW_CAP
                };
                self.highlighted = highlight::highlight_pair(
                    &file_diff.filename,
                    &old_text,
                    &new_text,
                    &self.config.syntax_theme,
                    Some(cap),
                )
                .map(|(old_hl, new_hl)| (to_tui_highlight(old_hl), to_tui_highlight(new_hl)));
            }
            self.aligned_rows = Some(rows);
        }
    }

    fn toggle_condensed(&mut self) {
        self.condensed = !self.condensed;
        self.vertical_scroll = 0;
        self.refresh_aligned_rows();
    }

    /// Condensed-or-full row order with inline agent notes spliced in
    /// after the rows they anchor to
    fn build_display_rows(&self, rows: &[AlignedRow]) -> Vec<side_by_side::DisplayRow> {
        use side_by_side::DisplayRow;

        let base: Vec<DisplayRow> = if self.condensed {
            side_by_side::condense(rows, CONDENSED_CONTEXT)
        } else {
            (0..rows.len()).map(DisplayRow::Row).collect()
        };

        let Some(file) = self.selected_file_path() else {
            return base;
        };
        let file = file.replace('\\', "/");
        if self.agent_session.notes.is_empty() {
            return base;
        }

        let mut result = Vec::with_capacity(base.len());
        for entry in base {
            result.push(entry);
            if let DisplayRow::Row(index) = entry {
                let row = &rows[index];
                for (note_index, note) in self.agent_session.notes.iter().enumerate() {
                    if note.file == file && note.anchors_to(row) {
                        let body_lines = self
                            .wrapped_notes
                            .get(note_index)
                            .map(|wrapped| wrapped.len())
                            .unwrap_or(1)
                            .max(1);
                        for line in 0..body_lines {
                            result.push(DisplayRow::Note {
                                note: note_index,
                                line,
                            });
                        }
                    }
                }
            }
        }
        result
    }

    /// Wrap (and fold) every note body to the current note pane width.
    /// The result backs both display splicing and rendering
    fn rebuild_wrapped_notes(&mut self, rows: &[AlignedRow]) {
        use unicode_width::UnicodeWidthStr;

        /// Notes longer than this many wrapped lines fold down...
        const NOTE_FOLD_THRESHOLD: usize = 6;
        /// ...to this many, plus an expander line
        const NOTE_FOLD_SHOWN: usize = 4;

        let gutter_width = rows
            .iter()
            .flat_map(|row| [row.old.as_ref(), row.new.as_ref()])
            .flatten()
            .map(|(number, _)| *number)
            .max()
            .unwrap_or(1)
            .to_string()
            .len()
            .max(3);
        let icon_width = match self.config.icon_mode {
            config::IconMode::Ascii => 1,
            _ => 2,
        };
        // Pane width minus the line-number gutter the notes align with
        let base = (self.last_note_wrap_width.max(24) as usize).saturating_sub(gutter_width + 1);

        self.wrapped_notes = self
            .agent_session
            .notes
            .iter()
            .map(|note| {
                let prefix = icon_width + 1 + note.author.width() + 2;
                let mut wrapped = agent::wrap_body(
                    &note.body,
                    base.saturating_sub(prefix),
                    base.saturating_sub(3),
                );
                if !self.notes_expanded && wrapped.len() > NOTE_FOLD_THRESHOLD {
                    let hidden = wrapped.len() - NOTE_FOLD_SHOWN;
                    wrapped.truncate(NOTE_FOLD_SHOWN);
                    wrapped.push(format!("... (+{hidden} lines - n: expand)"));
                }
                wrapped
            })
            .collect();
    }

    /// Re-wrap and re-splice notes into the current display without
    /// re-aligning or re-highlighting (cheap; used when replies arrive,
    /// the pane width changes, or folding toggles)
    fn refresh_note_display(&mut self) {
        let Some(rows) = self.aligned_rows.take() else {
            return;
        };
        self.rebuild_wrapped_notes(&rows);
        self.display_rows = self.build_display_rows(&rows);
        self.aligned_rows = Some(rows);
    }

    fn toggle_notes_expanded(&mut self) {
        self.notes_expanded = !self.notes_expanded;
        self.refresh_note_display();
    }

    fn selected_file_path(&self) -> Option<String> {
        self.get_current_file_tree_items()
            .get(self.selected_index)
            .filter(|item| !item.is_directory)
            .map(|item| item.full_path.clone())
    }

    /// Enter comment mode: a line cursor appears in the side-by-side
    /// view; j/k move it, Enter opens the input, Esc leaves
    fn enter_comment_mode(&mut self) {
        if self.view_mode == ViewMode::Unified || self.aligned_rows.is_none() {
            self.warning_message =
                Some("Comments work in the side-by-side/after views (v to switch)".to_string());
            return;
        }
        if self.selected_file_path().is_none() {
            self.warning_message = Some("Select a file to comment on".to_string());
            return;
        }
        let start = self.vertical_scroll as usize;
        let cursor = self
            .display_rows
            .iter()
            .enumerate()
            .skip(start)
            .find(|(_, entry)| matches!(entry, side_by_side::DisplayRow::Row(_)))
            .or_else(|| {
                self.display_rows
                    .iter()
                    .enumerate()
                    .find(|(_, entry)| matches!(entry, side_by_side::DisplayRow::Row(_)))
            })
            .map(|(index, _)| index);
        match cursor {
            Some(index) => {
                self.comment_cursor = Some(index);
                self.ensure_comment_cursor_visible();
            }
            None => self.warning_message = Some("No diff lines to comment on".to_string()),
        }
    }

    fn exit_comment_mode(&mut self) {
        self.comment_cursor = None;
        self.comment_anchor = None;
        self.comment_input_mode = false;
        self.comment_text.clear();
    }

    /// v in comment mode: drop or lift the range-selection anchor
    fn toggle_comment_anchor(&mut self) {
        self.comment_anchor = match self.comment_anchor {
            Some(_) => None,
            None => self.comment_cursor,
        };
    }

    /// Selected display-row span: anchor..cursor, or just the cursor
    fn comment_selection(&self) -> Option<(usize, usize)> {
        let cursor = self.comment_cursor?;
        let anchor = self.comment_anchor.unwrap_or(cursor);
        Some((anchor.min(cursor), anchor.max(cursor)))
    }

    /// Move the comment cursor by whole diff rows, skipping gap markers
    /// and note lines
    fn comment_cursor_move(&mut self, delta: isize, steps: usize) {
        for _ in 0..steps {
            let Some(current) = self.comment_cursor else {
                return;
            };
            let mut index = current as isize;
            loop {
                index += delta.signum();
                if index < 0 || index as usize >= self.display_rows.len() {
                    return;
                }
                if matches!(
                    self.display_rows[index as usize],
                    side_by_side::DisplayRow::Row(_)
                ) {
                    break;
                }
            }
            self.comment_cursor = Some(index as usize);
        }
        self.ensure_comment_cursor_visible();
    }

    fn comment_cursor_jump(&mut self, to_end: bool) {
        let position = if to_end {
            self.display_rows
                .iter()
                .rposition(|entry| matches!(entry, side_by_side::DisplayRow::Row(_)))
        } else {
            self.display_rows
                .iter()
                .position(|entry| matches!(entry, side_by_side::DisplayRow::Row(_)))
        };
        if let Some(index) = position {
            self.comment_cursor = Some(index);
            self.ensure_comment_cursor_visible();
        }
    }

    fn ensure_comment_cursor_visible(&mut self) {
        let Some(cursor) = self.comment_cursor else {
            return;
        };
        let height = self.last_diff_height.max(1) as usize;
        let top = self.vertical_scroll as usize;
        if cursor < top {
            self.vertical_scroll = cursor as u16;
        } else if cursor >= top + height {
            self.vertical_scroll = (cursor + 1 - height) as u16;
        }
    }

    /// Send the typed comment/question for the cursor row through the
    /// configured sink
    fn send_comment(&mut self) {
        let text = self.comment_text.trim().to_string();
        if text.is_empty() {
            self.warning_message = Some("Comment is empty".to_string());
            return;
        }
        let Some((span_start, span_end)) = self.comment_selection() else {
            return;
        };
        // Row indices covered by the selected display span (rows are
        // spliced in ascending order, so first/last bound the range)
        let selected_rows: Vec<usize> = self
            .display_rows
            .get(span_start..=span_end.min(self.display_rows.len().saturating_sub(1)))
            .unwrap_or_default()
            .iter()
            .filter_map(|entry| match entry {
                side_by_side::DisplayRow::Row(index) => Some(*index),
                _ => None,
            })
            .collect();
        let (Some(&first_row), Some(&last_row)) = (selected_rows.first(), selected_rows.last())
        else {
            return;
        };
        let Some(file) = self.selected_file_path() else {
            return;
        };
        let Some(rows) = self.aligned_rows.take() else {
            return;
        };

        let payload = self.agent_session.build_payload(
            self.comment_kind,
            &file,
            &rows,
            first_row,
            last_row,
            &text,
        );
        let result = self.agent_session.send(&self.config.agent, &payload);
        self.aligned_rows = Some(rows);

        match result {
            Ok(status) => {
                self.warning_message =
                    Some(format!("{} sent ({status})", self.comment_kind.label()));
                self.exit_comment_mode();
                self.refresh_note_display();
            }
            Err(e) => {
                // Keep the input open so the text is not lost
                self.warning_message = Some(format!("Send failed: {e}"));
            }
        }
    }

    /// Switch the file list between flat full-path and directory tree,
    /// keeping the current file selected
    fn toggle_flat_view(&mut self) {
        self.config.flat_file_list = !self.config.flat_file_list;

        let selected_path = self
            .get_current_file_tree_items()
            .get(self.selected_index)
            .map(|item| item.full_path.clone());

        self.rebuild_file_tree();
        if self.search_mode {
            self.update_search_filter();
        } else {
            self.filtered_file_tree_items = self.file_tree_items.clone();
        }

        let items = self.get_current_file_tree_items();
        self.selected_index = selected_path
            .and_then(|path| items.iter().position(|item| item.full_path == path))
            .unwrap_or(0);
        self.file_list_state.select(Some(self.selected_index));
        self.update_diff_content();
    }

    /// Route navigation keys to whichever list the left pane shows
    fn nav_next(&mut self) {
        match self.left_pane {
            LeftPane::Files => self.select_next(),
            LeftPane::History => self.history_move(1),
        }
    }

    fn nav_previous(&mut self) {
        match self.left_pane {
            LeftPane::Files => self.select_previous(),
            LeftPane::History => self.history_move(-1),
        }
    }

    /// Tab navigation wraps around at both ends of the list
    fn nav_next_wrapping(&mut self) {
        match self.left_pane {
            LeftPane::Files => {
                let len = self.get_current_file_tree_items().len();
                if len > 0 && self.selected_index + 1 >= len {
                    self.jump_to_top();
                } else {
                    self.select_next();
                }
            }
            LeftPane::History => {
                if self.commit_index + 1 >= self.history_len() {
                    self.history_select(0);
                } else {
                    self.history_move(1);
                }
            }
        }
    }

    fn nav_previous_wrapping(&mut self) {
        match self.left_pane {
            LeftPane::Files => {
                if self.selected_index == 0 {
                    self.jump_to_bottom();
                } else {
                    self.select_previous();
                }
            }
            LeftPane::History => {
                if self.commit_index == 0 {
                    self.history_select(self.history_len().saturating_sub(1));
                } else {
                    self.history_move(-1);
                }
            }
        }
    }

    fn nav_first(&mut self) {
        match self.left_pane {
            LeftPane::Files => self.jump_to_top(),
            LeftPane::History => self.history_select(0),
        }
    }

    fn nav_last(&mut self) {
        match self.left_pane {
            LeftPane::Files => self.jump_to_bottom(),
            LeftPane::History => self.history_select(self.history_len().saturating_sub(1)),
        }
    }

    fn handle_menu_action(&mut self, action: MenuAction) {
        match action {
            MenuAction::Files => self.show_files_pane(),
            MenuAction::History => self.open_history(),
            MenuAction::ToggleView => self.toggle_view_mode(),
            MenuAction::Help => self.show_help = !self.show_help,
            MenuAction::Quit => self.should_quit = true,
        }
    }

    fn handle_mouse_click(&mut self, mouse: MouseEvent) {
        let position = Position {
            x: mouse.column,
            y: mouse.row,
        };

        if self.show_help {
            self.show_help = false;
            return;
        }

        let menu_action = self
            .regions
            .menu_items
            .iter()
            .find(|(rect, _)| rect.contains(position))
            .map(|(_, action)| *action);
        if let Some(action) = menu_action {
            self.handle_menu_action(action);
            return;
        }

        if self.left_pane == LeftPane::History && self.regions.graph_area.contains(position) {
            self.handle_graph_click(mouse.row);
            return;
        }

        if self.regions.list_area.contains(position) {
            self.handle_list_click(mouse.row);
        }
    }

    /// Click on a graph row selects its commit; a second click opens it
    fn handle_graph_click(&mut self, row: u16) {
        let area = self.regions.graph_area;
        if row <= area.y || row >= area.y + area.height.saturating_sub(1) {
            return;
        }
        let index = self.graph_scroll + (row - area.y - 1) as usize;
        let Some(hash) = self.graph_rows.get(index).and_then(|r| r.hash.clone()) else {
            return;
        };
        let Some(position) = self.commits.iter().position(|c| c.hash == hash) else {
            return;
        };
        if self.commit_index == position + 1 {
            self.open_selected_history_entry();
        } else {
            self.history_select(position + 1);
        }
    }

    fn handle_list_click(&mut self, row: u16) {
        let area = self.regions.list_area;
        // Only rows inside the borders map to list entries
        if row <= area.y || row >= area.y + area.height.saturating_sub(1) {
            return;
        }
        let visual_row = (row - area.y - 1) as usize;

        match self.left_pane {
            LeftPane::History => {
                let index = self.commit_list_state.offset() + visual_row;
                if index >= self.history_len() {
                    return;
                }
                if index == self.commit_index {
                    // Second click on the selected entry opens it
                    self.open_selected_history_entry();
                } else {
                    self.history_select(index);
                }
            }
            LeftPane::Files => {
                let index = self.file_list_state.offset() + visual_row;
                let Some(item) = self.get_current_file_tree_items().get(index).cloned() else {
                    return;
                };
                self.selected_index = index;
                self.file_list_state.select(Some(index));
                if item.is_directory {
                    self.toggle_directory();
                } else {
                    self.update_diff_content();
                }
            }
        }
    }

    fn handle_mouse_scroll(&mut self, mouse: MouseEvent, scroll_down: bool) {
        let position = Position {
            x: mouse.column,
            y: mouse.row,
        };
        let over_graph =
            self.left_pane == LeftPane::History && self.regions.graph_area.contains(position);
        if self.regions.left_column.contains(position) || over_graph {
            if scroll_down {
                self.nav_next();
            } else {
                self.nav_previous();
            }
        } else if scroll_down {
            self.scroll_down(3);
        } else {
            self.scroll_up(3);
        }
    }

    fn select_next(&mut self) {
        let current_items = self.get_current_file_tree_items();
        if !current_items.is_empty() && self.selected_index < current_items.len() - 1 {
            self.selected_index += 1;
            self.file_list_state.select(Some(self.selected_index));
            self.update_diff_content();
        }
    }

    fn select_previous(&mut self) {
        if self.selected_index > 0 {
            self.selected_index -= 1;
            self.file_list_state.select(Some(self.selected_index));
            self.update_diff_content();
        }
    }

    fn update_diff_content(&mut self) {
        // The comment cursor points into the old file's rows
        self.exit_comment_mode();
        let current_items = self.get_current_file_tree_items();
        if let Some(tree_item) = current_items.get(self.selected_index) {
            if let Some(file_diff) = &tree_item.file_diff {
                // Try to get individual file diff if we have a git executor
                if let Some(ref git_executor) = self.git_executor {
                    match git_executor.get_file_diff(&self.operation_mode, &tree_item.full_path) {
                        Ok(fresh_diff) => {
                            self.diff_output = fresh_diff;
                        }
                        Err(_) => {
                            // Fallback to stored diff content
                            self.diff_output = file_diff.content.clone();
                        }
                    }
                } else {
                    // Use stored diff content
                    self.diff_output = file_diff.content.clone();
                }

                // Apply external diff tool if configured (unified view only;
                // the side-by-side panes render file contents in-app)
                if self.view_mode == ViewMode::Unified {
                    // Use terminal width for proper side-by-side display (lazygit style)
                    if let Ok((terminal_width, _)) = crossterm::terminal::size() {
                        self.apply_external_diff_tool_with_width(Some(terminal_width));
                    } else {
                        self.apply_external_diff_tool();
                    }
                } else {
                    self.warning_message = None;
                }

                // Reset scroll position when switching files
                self.vertical_scroll = 0;
                self.horizontal_scroll = 0;
            } else {
                // Directory selected - show directory info
                self.diff_output = format!("Directory: {}", tree_item.full_path);
                self.vertical_scroll = 0;
                self.horizontal_scroll = 0;
            }
        }

        self.refresh_aligned_rows();
    }

    fn apply_external_diff_tool(&mut self) {
        self.apply_external_diff_tool_with_width(None);
    }

    fn apply_external_diff_tool_with_width(&mut self, width: Option<u16>) {
        // Check if we should use a diff tool (pager or external)
        match self.config.get_diff_command_type() {
            DiffCommandType::GitDefault => {
                // No processing needed
            }
            DiffCommandType::Pager(_) | DiffCommandType::External(_) => {
                match self.execute_external_diff_tool_with_width(&self.diff_output, width) {
                    Ok(processed_output) => {
                        self.diff_output = processed_output;
                        self.warning_message = None;
                    }
                    Err(e) => {
                        self.warning_message =
                            Some(format!("Failed to process with diff tool: {e}"));
                    }
                }
            }
        }
    }

    #[allow(dead_code)]
    fn execute_external_diff_tool(&self, diff_content: &str) -> Result<String> {
        self.execute_external_diff_tool_with_width(diff_content, None)
    }

    fn execute_external_diff_tool_with_width(
        &self,
        diff_content: &str,
        width: Option<u16>,
    ) -> Result<String> {
        let diff_command_type = self.config.get_diff_command_type();

        match diff_command_type {
            DiffCommandType::GitDefault => {
                Ok(diff_content.to_string()) // No processing needed
            }
            DiffCommandType::Pager(ref cmd) => {
                // Use stdin-based approach for pagers (delta, bat, ydiff, etc.)
                self.execute_pager_with_stdin_legacy(cmd, diff_content, width)
            }
            DiffCommandType::External(ref cmd) => {
                // Use Git's external diff mechanism for external diff tools like difftastic
                if let Some(w) = width {
                    self.execute_external_diff_via_git(cmd, w.saturating_sub(2), w)
                } else {
                    // Fallback with default widths
                    if let Ok((terminal_width, _)) = crossterm::terminal::size() {
                        self.execute_external_diff_via_git(
                            cmd,
                            terminal_width.saturating_sub(2),
                            terminal_width,
                        )
                    } else {
                        self.execute_external_diff_via_git(cmd, 78, 80)
                    }
                }
            }
        }
    }

    /// Common helper to execute external command with stdin input
    fn execute_command_with_stdin(
        &self,
        command_str: &str,
        input: &str,
        env_vars: &[(&str, String)],
    ) -> Result<String> {
        use std::io::Write;

        // Parse command and arguments
        let parts: Vec<&str> = command_str.split_whitespace().collect();
        if parts.is_empty() {
            return Err(anyhow::anyhow!("Empty command"));
        }

        let command_name = parts[0];
        let mut cmd = Command::new(command_name);

        // Add arguments
        if parts.len() > 1 {
            cmd.args(&parts[1..]);
        }

        // Set environment variables
        for (key, value) in env_vars {
            cmd.env(key, value);
        }

        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = cmd
            .spawn()
            .map_err(|e| anyhow::anyhow!("Failed to spawn {}: {}", command_name, e))?;

        // Write input
        if let Some(stdin) = child.stdin.take() {
            let mut writer = std::io::BufWriter::new(stdin);
            writer
                .write_all(input.as_bytes())
                .map_err(|e| anyhow::anyhow!("Failed to write to command: {}", e))?;
            writer
                .flush()
                .map_err(|e| anyhow::anyhow!("Failed to flush command input: {}", e))?;
        }

        let output = child
            .wait_with_output()
            .map_err(|e| anyhow::anyhow!("Failed to read from command: {}", e))?;

        if output.status.success() {
            String::from_utf8(output.stdout)
                .map_err(|e| anyhow::anyhow!("Command output is not valid UTF-8: {}", e))
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr);
            Err(anyhow::anyhow!("Command failed: {}", stderr))
        }
    }

    /// Legacy pager execution for backward compatibility with existing tools
    fn execute_pager_with_stdin_legacy(
        &self,
        command_str: &str,
        diff_content: &str,
        width: Option<u16>,
    ) -> Result<String> {
        // Apply template variable substitution
        let final_command_str = if let Some(w) = width {
            let content_width = w.saturating_sub(2);
            self.resolve_template_variables(command_str, content_width)
        } else {
            command_str.to_string()
        };

        // Prepare environment variables
        let mut env_vars = vec![
            ("TERM", DEFAULT_TERMINAL_TYPE.to_string()),
            ("LINES", DEFAULT_TERMINAL_HEIGHT.to_string()),
        ];

        if let Some(w) = width {
            env_vars.push(("COLUMNS", w.to_string()));
        }

        self.execute_command_with_stdin(&final_command_str, diff_content, &env_vars)
    }

    fn execute_external_diff_tool_with_area_width(
        &self,
        diff_content: &str,
        area_width: u16,
        terminal_width: u16,
    ) -> Result<String> {
        let diff_command_type = self.config.get_diff_command_type();

        match diff_command_type {
            DiffCommandType::GitDefault => {
                Ok(diff_content.to_string()) // No processing needed
            }
            DiffCommandType::Pager(ref cmd) => {
                // Use stdin-based approach for pagers
                self.execute_pager_with_stdin(cmd, diff_content, area_width, terminal_width)
            }
            DiffCommandType::External(ref cmd) => {
                // Use Git's external diff mechanism for external diff tools like difftastic
                self.execute_external_diff_via_git(cmd, area_width, terminal_width)
            }
        }
    }

    /// Execute pager commands via stdin (delta, bat, ydiff, etc.)
    fn execute_pager_with_stdin(
        &self,
        command_str: &str,
        diff_content: &str,
        area_width: u16,
        terminal_width: u16,
    ) -> Result<String> {
        // Apply template variable substitution with both area and terminal width
        let final_command_str = self.resolve_template_variables_with_area_width(
            command_str,
            area_width,
            terminal_width,
        );

        // Prepare environment variables
        let env_vars = vec![
            ("TERM", DEFAULT_TERMINAL_TYPE.to_string()),
            ("COLUMNS", terminal_width.to_string()),
            ("LINES", DEFAULT_TERMINAL_HEIGHT.to_string()),
        ];

        self.execute_command_with_stdin(&final_command_str, diff_content, &env_vars)
    }

    /// Setup essential environment variables for Git external diff tools
    fn setup_git_external_diff_env(
        &self,
        cmd: &mut Command,
        _area_width: u16,
        terminal_width: u16,
    ) {
        // Essential terminal environment only
        cmd.env("TERM", DEFAULT_TERMINAL_TYPE);
        cmd.env("COLUMNS", terminal_width.to_string());
        cmd.env("LINES", DEFAULT_TERMINAL_HEIGHT);
    }

    /// Execute external diff tools via Git's external diff mechanism
    fn execute_external_diff_via_git(
        &self,
        command_str: &str,
        area_width: u16,
        terminal_width: u16,
    ) -> Result<String> {
        use std::process::{Command, Stdio};

        // Apply template variable substitution
        let final_command_str = self.resolve_template_variables_with_area_width(
            command_str,
            area_width,
            terminal_width,
        );

        // Get current file path if available
        let current_items = self.get_current_file_tree_items();
        let file_path = if let Some(tree_item) = current_items.get(self.selected_index) {
            if !tree_item.is_directory {
                Some(&tree_item.full_path)
            } else {
                None
            }
        } else {
            None
        };

        if file_path.is_none() {
            return Err(anyhow::anyhow!("No file selected for external diff"));
        }

        // Build git command using external diff mechanism (like lazygit)
        let mut cmd = Command::new("git");
        let external_diff_config = format!("diff.external={final_command_str}");

        cmd.args([
            "-c",
            &external_diff_config,
            "-c",
            "diff.noprefix=false",
            "diff",
            "--ext-diff",
            "--color=always",
        ]);

        // Add operation mode specific arguments
        match &self.operation_mode {
            OperationMode::GitWorkingDirectory => {
                // Compare working directory with index
            }
            OperationMode::GitCached => {
                cmd.arg("--cached");
            }
            OperationMode::Compare { target1, target2 } => {
                cmd.arg(target1);
                cmd.arg(target2);
            }
            OperationMode::GitDiff { target } => {
                cmd.arg(target);
            }
            _ => {
                return Err(anyhow::anyhow!(
                    "External diff not supported for this operation mode"
                ));
            }
        }

        // Add specific file path
        cmd.arg("--");
        cmd.arg(file_path.unwrap());

        // Set environment variables for git and child processes
        self.setup_git_external_diff_env(&mut cmd, area_width, terminal_width);

        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

        let output = cmd
            .output()
            .map_err(|e| anyhow::anyhow!("Failed to execute git with external diff: {}", e))?;

        if output.status.success() {
            String::from_utf8(output.stdout)
                .map_err(|e| anyhow::anyhow!("Git external diff output is not valid UTF-8: {}", e))
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr);
            Err(anyhow::anyhow!("Git external diff failed: {}", stderr))
        }
    }

    fn scroll_up(&mut self, amount: u16) {
        self.vertical_scroll = self.vertical_scroll.saturating_sub(amount);
        // No need to clamp here - it will be clamped in render
    }

    fn scroll_down(&mut self, amount: u16) {
        self.vertical_scroll = self.vertical_scroll.saturating_add(amount);
        // No need to clamp here - it will be clamped in render
    }

    fn scroll_left(&mut self, amount: u16) {
        self.horizontal_scroll = self.horizontal_scroll.saturating_sub(amount);
        // No need to clamp here - it will be clamped in render
    }

    fn scroll_right(&mut self, amount: u16) {
        self.horizontal_scroll = self.horizontal_scroll.saturating_add(amount);
        // No need to clamp here - it will be clamped in render
    }

    fn jump_to_top(&mut self) {
        self.selected_index = 0;
        self.file_list_state.select(Some(self.selected_index));
        self.update_diff_content();
    }

    fn jump_to_bottom(&mut self) {
        let current_items = self.get_current_file_tree_items();
        if !current_items.is_empty() {
            self.selected_index = current_items.len() - 1;
            self.file_list_state.select(Some(self.selected_index));
            self.update_diff_content();
        }
    }

    fn toggle_file_checked(&mut self) {
        let current_items = if self.search_mode {
            &self.filtered_file_tree_items
        } else {
            &self.file_tree_items
        };

        if let Some(tree_item) = current_items.get(self.selected_index) {
            // Only toggle check state for files, not directories
            if !tree_item.is_directory {
                let file_path = tree_item.full_path.clone();
                let was_checked = self.checked_files.contains(&file_path);

                if was_checked {
                    self.checked_files.remove(&file_path);
                } else {
                    self.checked_files.insert(file_path.clone());
                }

                // Save to persistence if we have a diff key
                if let Some(file_diff) = tree_item.file_diff.as_ref() {
                    if let Some(diff_key) = &file_diff.diff_key {
                        let is_now_checked = !was_checked;
                        if let Err(e) = self
                            .persistence_manager
                            .save_check_state(diff_key, is_now_checked)
                        {
                            self.warning_message = Some(format!("Failed to save check state: {e}"));
                        }
                    }
                }
            }
        }
    }

    fn get_current_file_tree_items(&self) -> &Vec<FileTreeItem> {
        if self.search_mode {
            &self.filtered_file_tree_items
        } else {
            &self.file_tree_items
        }
    }

    fn enter_search_mode(&mut self) {
        if self.search_mode {
            // Already in search mode, clear query and start fresh input
            self.search_query.clear();
            self.search_input_mode = true;
            self.selected_index = 0;
            self.file_list_state.select(Some(self.selected_index));
            self.update_search_filter();
        } else {
            // Enter search mode for the first time
            self.search_mode = true;
            self.search_input_mode = true;
            self.search_query.clear();
            self.selected_index = 0;
            self.file_list_state.select(Some(self.selected_index));
            self.update_search_filter();
        }
    }

    fn exit_search_mode(&mut self) {
        self.search_mode = false;
        self.search_input_mode = false;
        self.search_query.clear();
        self.selected_index = 0;
        self.file_list_state.select(Some(self.selected_index));
        self.update_diff_content();
    }

    fn confirm_search(&mut self) {
        self.search_input_mode = false;
        // Keep search_mode = true to show filtered results
        // But allow navigation with hjkl
    }

    fn add_search_char(&mut self, c: char) {
        if self.search_input_mode {
            self.search_query.push(c);
            self.update_search_filter();
        }
    }

    fn remove_search_char(&mut self) {
        if self.search_input_mode && !self.search_query.is_empty() {
            self.search_query.pop();
            self.update_search_filter();
        }
    }

    fn update_search_filter(&mut self) {
        if self.search_query.is_empty() {
            self.filtered_file_tree_items = self.file_tree_items.clone();
        } else {
            // Simple fuzzy matching - each character in query should appear in order
            self.filtered_file_tree_items = self
                .file_tree_items
                .iter()
                .filter(|item| self.fuzzy_match(&item.full_path, &self.search_query))
                .cloned()
                .collect();
        }

        // Reset selection and update diff content
        self.selected_index = 0;
        self.file_list_state.select(Some(self.selected_index));
        self.update_diff_content();
    }

    fn fuzzy_match(&self, text: &str, pattern: &str) -> bool {
        // Simple substring matching like diffnav
        text.to_lowercase().contains(&pattern.to_lowercase())
    }

    fn toggle_directory(&mut self) {
        if let Some(tree_item) = self.file_tree_items.get(self.selected_index) {
            if tree_item.is_directory {
                let path = tree_item.full_path.clone();
                if self.collapsed_directories.contains(&path) {
                    self.collapsed_directories.remove(&path);
                } else {
                    self.collapsed_directories.insert(path);
                }
                // Rebuild the tree with updated collapsed state
                self.rebuild_file_tree();
            }
        }
    }

    fn rebuild_file_tree(&mut self) {
        // Use original file diffs instead of extracting from current items
        self.file_tree_items = if self.config.flat_file_list {
            FileTreeBuilder::build_flat_list(&self.original_file_diffs)
        } else {
            FileTreeBuilder::build_file_tree_with_collapsed(
                &self.original_file_diffs,
                &self.collapsed_directories,
            )
        };

        // Adjust selected index if needed
        if self.selected_index >= self.file_tree_items.len() {
            self.selected_index = self.file_tree_items.len().saturating_sub(1);
            self.file_list_state.select(Some(self.selected_index));
        }
    }

    /// Refresh diff output with specific width for side-by-side display
    fn refresh_diff_with_width(&mut self, width: u16) {
        // Re-execute diff tool with the new width for proper side-by-side alignment
        match self.config.get_diff_command_type() {
            DiffCommandType::GitDefault => {
                // No processing needed for default git diff
            }
            DiffCommandType::Pager(_) | DiffCommandType::External(_) => {
                let current_items = self.get_current_file_tree_items();
                if let Some(tree_item) = current_items.get(self.selected_index) {
                    if let Some(file_diff) = &tree_item.file_diff {
                        // Get fresh diff content for the current file
                        let base_diff = if let Some(ref git_executor) = self.git_executor {
                            match git_executor
                                .get_file_diff(&self.operation_mode, &tree_item.full_path)
                            {
                                Ok(fresh_diff) => fresh_diff,
                                Err(_) => file_diff.content.clone(),
                            }
                        } else {
                            file_diff.content.clone()
                        };

                        // Apply diff tool with width
                        match self.execute_external_diff_tool_with_width(&base_diff, Some(width)) {
                            Ok(processed_output) => {
                                self.diff_output = processed_output;
                                self.warning_message = None;
                            }
                            Err(e) => {
                                self.warning_message =
                                    Some(format!("Failed to refresh diff with width: {e}"));
                            }
                        }
                    }
                }
            }
        }
    }

    /// Refresh diff output with area width and terminal width for better template calculations
    fn refresh_diff_with_area_width(&mut self, area_width: u16, terminal_width: u16) {
        match self.config.get_diff_command_type() {
            DiffCommandType::GitDefault => {
                // No processing needed for default git diff
            }
            DiffCommandType::Pager(_) | DiffCommandType::External(_) => {
                let current_items = self.get_current_file_tree_items();
                if let Some(tree_item) = current_items.get(self.selected_index) {
                    if let Some(file_diff) = &tree_item.file_diff {
                        // Get fresh diff content for the current file
                        let base_diff = if let Some(ref git_executor) = self.git_executor {
                            match git_executor
                                .get_file_diff(&self.operation_mode, &tree_item.full_path)
                            {
                                Ok(fresh_diff) => fresh_diff,
                                Err(_) => file_diff.content.clone(),
                            }
                        } else {
                            file_diff.content.clone()
                        };

                        // Execute diff tool with area width for optimal template variable usage
                        match self.execute_external_diff_tool_with_area_width(
                            &base_diff,
                            area_width,
                            terminal_width,
                        ) {
                            Ok(processed_output) => {
                                self.diff_output = processed_output;
                                self.warning_message = None;
                            }
                            Err(e) => {
                                self.warning_message =
                                    Some(format!("Failed to refresh diff with area width: {e}"));
                            }
                        }
                    }
                }
            }
        }
    }

    /// Clamp scroll values to valid ranges based on content and viewport size
    fn clamp_scroll(&mut self, viewport_height: u16, viewport_width: u16) {
        // Calculate content dimensions
        let content_height = self.diff_output.lines().count() as u16;

        // Calculate the maximum display width, accounting for ANSI escape sequences
        let max_line_width = self
            .diff_output
            .lines()
            .map(|line| self.calculate_display_width(line))
            .max()
            .unwrap_or(0) as u16;

        // Account for borders (subtract 2 for top and bottom borders)
        let available_height = viewport_height.saturating_sub(2);
        let available_width = viewport_width.saturating_sub(2);

        // Vertical scroll limit: can't scroll beyond content
        let max_vertical_scroll = content_height.saturating_sub(available_height);

        // Horizontal scroll limit: can't scroll beyond the longest line
        let max_horizontal_scroll = max_line_width.saturating_sub(available_width);

        // Clamp the scroll values
        self.vertical_scroll = self.vertical_scroll.min(max_vertical_scroll);
        self.horizontal_scroll = self.horizontal_scroll.min(max_horizontal_scroll);
    }

    /// Calculate the display width of a line, excluding ANSI escape sequences
    fn calculate_display_width(&self, line: &str) -> usize {
        // Use strip_ansi_escapes to remove ANSI sequences, then calculate width
        if self.contains_ansi_codes(line) {
            let stripped = strip_ansi_escapes::strip(line);
            // Convert to string and calculate width
            match String::from_utf8(stripped) {
                Ok(clean_line) => self.calculate_text_width(&clean_line),
                Err(_) => line.len(), // Fallback to raw length
            }
        } else {
            self.calculate_text_width(line)
        }
    }

    /// Calculate the display width of plain text (no ANSI sequences)
    fn calculate_text_width(&self, text: &str) -> usize {
        text.chars()
            .map(|ch| {
                if ch == '\t' {
                    4 // Tab character: assume 4 spaces
                } else if ch.is_control() {
                    0 // Skip control characters
                } else {
                    1 // Regular character
                }
            })
            .sum()
    }

    /// Check if a string contains ANSI escape sequences
    pub fn contains_ansi_codes(&self, text: &str) -> bool {
        text.contains('\x1b') || text.contains("\u{001b}")
    }

    /// Calculate template variable values
    fn calculate_template_values(&self, area_width: u16, terminal_width: u16) -> TemplateValues {
        let diff_area_width = area_width.saturating_sub(2); // Remove borders
        let column_width = (terminal_width / 2).saturating_sub(6);
        let diff_column_width = (diff_area_width / 2).saturating_sub(6);

        TemplateValues {
            width: terminal_width,
            column_width,
            diff_area_width,
            diff_column_width,
        }
    }

    /// Apply template variable substitutions to command string
    fn apply_template_substitutions(&self, command_str: &str, values: &TemplateValues) -> String {
        let mut result = command_str.to_string();

        // Replace all template variable variants
        let substitutions = [
            ("{{width}}", values.width.to_string()),
            ("{{.width}}", values.width.to_string()),
            ("{{columnWidth}}", values.column_width.to_string()),
            ("{{.columnWidth}}", values.column_width.to_string()),
            ("{{diffAreaWidth}}", values.diff_area_width.to_string()),
            ("{{.diffAreaWidth}}", values.diff_area_width.to_string()),
            ("{{diffColumnWidth}}", values.diff_column_width.to_string()),
            ("{{.diffColumnWidth}}", values.diff_column_width.to_string()),
        ];

        for (template, value) in &substitutions {
            result = result.replace(template, value);
        }

        result
    }

    /// Resolve template variables in command string (lazygit style)
    fn resolve_template_variables(&self, command_str: &str, width: u16) -> String {
        let area_width = (width * 80 / 100).saturating_sub(2); // 80% minus borders
        let values = self.calculate_template_values(area_width, width);
        self.apply_template_substitutions(command_str, &values)
    }

    /// Resolve template variables with separate area and terminal widths for better precision
    fn resolve_template_variables_with_area_width(
        &self,
        command_str: &str,
        area_width: u16,
        terminal_width: u16,
    ) -> String {
        let values = self.calculate_template_values(area_width, terminal_width);
        self.apply_template_substitutions(command_str, &values)
    }
}

fn main() -> Result<()> {
    // Parse command line arguments
    let cli = Cli::parse_args();

    // Shell completions never enter mode dispatch
    if let Some(Commands::Completions { shell }) = &cli.command {
        generate_completions(*shell);
        return Ok(());
    }

    let operation_mode = cli.get_operation_mode();

    // Handle invalid arguments first
    if let OperationMode::Invalid { reason } = &operation_mode {
        eprintln!("Error: {reason}");
        std::process::exit(1);
    }

    // Load configuration
    let mut config = if let Some(config_path) = cli.config {
        Config::load_from_path(&config_path)?
    } else {
        Config::load()?
    };
    if cli.flat {
        config.flat_file_list = true;
    }
    if let Some(icon_mode) = cli.icons {
        config.icon_mode = icon_mode;
    }

    // Check if we need a git repository
    if operation_mode.requires_git_repo() && !GitExecutor::is_git_repo() {
        return Err(anyhow::anyhow!("Not in a git repository"));
    }

    // Get diff data based on operation mode
    let is_stdin_terminal = io::IsTerminal::is_terminal(&io::stdin());
    if cli.verbose {
        eprintln!("Debug: stdin is terminal: {is_stdin_terminal}");
        eprintln!("Debug: operation mode: {operation_mode:?}");
    }

    let file_diffs = if !is_stdin_terminal {
        // Stdin mode: read piped input (backward compatibility)
        if cli.verbose {
            eprintln!("Debug: Using stdin mode");
        }
        read_input_completely().unwrap_or_else(|_| {
            if cli.verbose {
                eprintln!("Debug: No stdin input, falling back to git executor");
            }
            get_diffs_from_git(&operation_mode).unwrap_or_default()
        })
    } else {
        // Interactive mode: use git executor
        if cli.verbose {
            eprintln!("Debug: Using git executor mode");
        }
        get_diffs_from_git(&operation_mode)?
    };

    // A repository with no pending changes still opens, straight into the
    // history browser; outside a repository there is nothing to show
    if file_diffs.is_empty() && !GitExecutor::is_git_repo() {
        println!("No differences found.");
        return Ok(());
    }

    // Initialize TUI
    enable_raw_mode()
        .map_err(|e| anyhow::anyhow!("Failed to initialize terminal raw mode: {}", e))?;

    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let no_changes = file_diffs.is_empty();
    let mut app = App::new(config, file_diffs, operation_mode)?;
    if no_changes {
        app.open_history();
    }
    let res = run_app(&mut terminal, app);

    // Restore terminal
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;

    if let Err(err) = res {
        eprintln!("{err:?}")
    }

    Ok(())
}

fn generate_completions(shell: clap_complete::Shell) {
    use clap::CommandFactory;
    use clap_complete::{Generator, generate};
    use std::io;

    fn print_completions<G: Generator>(generator: G, cmd: &mut clap::Command) {
        generate(
            generator,
            cmd,
            cmd.get_name().to_string(),
            &mut io::stdout(),
        );
    }

    let mut cmd = Cli::command();
    print_completions(shell, &mut cmd);
}

fn get_diffs_from_git(mode: &OperationMode) -> Result<Vec<FileDiff>> {
    let git_executor = GitExecutor::new();

    // Get overall diff output
    let diff_output = git_executor.get_diff(mode)?;

    if diff_output.is_empty() {
        return Ok(vec![]);
    }

    // Parse the diff output to get individual file diffs
    Ok(DiffParser::parse(&diff_output))
}

fn read_input_completely() -> Result<Vec<FileDiff>> {
    // Read all stdin content at once
    let mut buffer = String::new();
    io::stdin()
        .read_to_string(&mut buffer)
        .map_err(|e| anyhow::anyhow!("Failed to read from stdin: {}", e))?;

    if buffer.trim().is_empty() {
        anyhow::bail!("No input received from stdin");
    }

    Ok(DiffParser::parse(&buffer))
}

/// Resolve a listed file to its on-disk location: relative to the
/// invocation directory first, then the repository root
fn resolve_editor_path(path: &str) -> Option<std::path::PathBuf> {
    let direct = std::path::PathBuf::from(path);
    if direct.exists() {
        return Some(direct);
    }
    if let Ok(root) = GitExecutor::toplevel() {
        let joined = root.join(path);
        if joined.exists() {
            return Some(joined);
        }
    }
    None
}

/// Editor to launch: config `editor`, then $EDITOR, then the OS default
/// (file association on Windows, vi elsewhere)
fn editor_command(config_editor: &str, file: &std::path::Path) -> (String, Vec<String>) {
    let file_arg = file.display().to_string();
    let from_spec = |spec: &str| {
        let mut parts: Vec<String> = spec.split_whitespace().map(String::from).collect();
        let program = parts.remove(0);
        parts.push(file_arg.clone());
        (program, parts)
    };

    let configured = config_editor.trim();
    if !configured.is_empty() {
        return from_spec(configured);
    }
    if let Ok(editor) = std::env::var("EDITOR") {
        if !editor.trim().is_empty() {
            return from_spec(editor.trim());
        }
    }
    if cfg!(windows) {
        (
            "cmd".to_string(),
            vec![
                "/c".to_string(),
                "start".to_string(),
                String::new(),
                file_arg,
            ],
        )
    } else {
        ("vi".to_string(), vec![file_arg])
    }
}

/// Open the selected file in an editor, suspending the TUI while a
/// terminal editor (vim, nano, ...) has the screen
fn open_selected_in_editor<B: ratatui::backend::Backend>(
    terminal: &mut Terminal<B>,
    app: &mut App,
) -> Result<()> {
    let Some(item) = app
        .get_current_file_tree_items()
        .get(app.selected_index)
        .cloned()
    else {
        return Ok(());
    };
    if item.is_directory {
        return Ok(());
    }
    let Some(path) = resolve_editor_path(&item.full_path) else {
        app.warning_message = Some(format!("File not found on disk: {}", item.full_path));
        return Ok(());
    };
    let (program, args) = editor_command(&app.config.editor, &path);

    disable_raw_mode()?;
    execute!(io::stdout(), LeaveAlternateScreen, DisableMouseCapture)?;
    let status = Command::new(&program).args(&args).status();
    enable_raw_mode()?;
    execute!(io::stdout(), EnterAlternateScreen, EnableMouseCapture)?;
    terminal.clear()?;

    match status {
        Ok(_) => {
            if app.is_working_tree_mode() {
                app.reload_diffs();
            }
        }
        Err(e) => app.warning_message = Some(format!("Failed to launch {program}: {e}")),
    }
    Ok(())
}

fn run_app<B: ratatui::backend::Backend>(terminal: &mut Terminal<B>, mut app: App) -> Result<()> {
    loop {
        terminal.draw(|f| ui(f, &mut app))?;

        // Use poll to handle the case where stdin might not be available
        if event::poll(std::time::Duration::from_millis(100))? {
            match event::read()? {
                Event::Key(key) => {
                    if key.kind == KeyEventKind::Release {
                        continue;
                    }
                    // Commit message box captures all typing while open
                    if app.commit_input_mode {
                        match key.code {
                            KeyCode::Enter => app.execute_commit(),
                            KeyCode::Esc => {
                                app.commit_input_mode = false;
                                app.commit_message.clear();
                            }
                            KeyCode::Backspace => {
                                app.commit_message.pop();
                            }
                            KeyCode::Char(c) => app.commit_message.push(c),
                            _ => {}
                        }
                        continue;
                    }
                    // Comment input box captures all typing while open
                    if app.comment_input_mode {
                        match key.code {
                            KeyCode::Enter => app.send_comment(),
                            KeyCode::Esc => {
                                app.comment_input_mode = false;
                                app.comment_text.clear();
                            }
                            KeyCode::Tab => app.comment_kind = app.comment_kind.toggle(),
                            KeyCode::Backspace => {
                                app.comment_text.pop();
                            }
                            KeyCode::Char(c) => app.comment_text.push(c),
                            _ => {}
                        }
                        continue;
                    }
                    // Comment mode: the line cursor owns navigation keys
                    if app.comment_cursor.is_some() {
                        match key.code {
                            // Esc lifts a range anchor first, then leaves
                            KeyCode::Esc if app.comment_anchor.is_some() => {
                                app.comment_anchor = None
                            }
                            KeyCode::Esc | KeyCode::Char('q') => app.exit_comment_mode(),
                            KeyCode::Char('v') => app.toggle_comment_anchor(),
                            KeyCode::Down | KeyCode::Char('j') => app.comment_cursor_move(1, 1),
                            KeyCode::Up | KeyCode::Char('k') => app.comment_cursor_move(-1, 1),
                            KeyCode::Char('d') | KeyCode::PageDown => {
                                app.comment_cursor_move(1, 10)
                            }
                            KeyCode::Char('u') | KeyCode::PageUp => app.comment_cursor_move(-1, 10),
                            KeyCode::Char('g') => app.comment_cursor_jump(false),
                            KeyCode::Char('G') => app.comment_cursor_jump(true),
                            KeyCode::Enter | KeyCode::Char('c') => {
                                app.comment_input_mode = true;
                            }
                            _ => {}
                        }
                        continue;
                    }
                    match key.code {
                        // Quit or exit search mode
                        KeyCode::Char('q') => {
                            if app.search_mode {
                                app.exit_search_mode();
                            } else {
                                app.should_quit = true;
                            }
                        }
                        KeyCode::Esc => {
                            if app.show_help {
                                app.show_help = false;
                            } else if app.search_mode {
                                app.exit_search_mode();
                            } else if app.left_pane == LeftPane::History {
                                app.show_files_pane();
                            } else if GitExecutor::is_git_repo() {
                                // Esc from Files steps back into the history list
                                app.open_history();
                            } else {
                                app.should_quit = true;
                            }
                        }

                        // Search mode (use '/' key, file list only)
                        KeyCode::Char('/')
                            if !app.search_input_mode && app.left_pane == LeftPane::Files =>
                        {
                            app.enter_search_mode();
                        }

                        // Enter to confirm search
                        KeyCode::Enter if app.search_input_mode => {
                            app.confirm_search();
                        }

                        // Backspace in search input mode
                        KeyCode::Backspace if app.search_input_mode => {
                            app.remove_search_char();
                        }

                        // Force a full repaint for terminals that garbled the
                        // screen (embedded xterms, resizes, ...)
                        KeyCode::Char('l') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                            terminal.clear()?;
                        }

                        // Editor-style diff scrolling; the History pane keeps
                        // j/k for commit selection
                        KeyCode::Down | KeyCode::Char('j') if !app.search_input_mode => {
                            if app.left_pane == LeftPane::History {
                                app.history_move(1);
                            } else {
                                app.scroll_down(1);
                            }
                        }
                        KeyCode::Up | KeyCode::Char('k') if !app.search_input_mode => {
                            if app.left_pane == LeftPane::History {
                                app.history_move(-1);
                            } else {
                                app.scroll_up(1);
                            }
                        }

                        // Tab / Shift+Tab move through the left pane list,
                        // wrapping around at either end
                        KeyCode::Tab if !app.search_input_mode => app.nav_next_wrapping(),
                        KeyCode::BackTab if !app.search_input_mode => app.nav_previous_wrapping(),

                        // Handle character input in search input mode (must be after other char handlers)
                        KeyCode::Char(c) if app.search_input_mode => {
                            app.add_search_char(c);
                        }
                        KeyCode::Enter => {
                            if app.left_pane == LeftPane::History {
                                app.open_selected_history_entry();
                            } else if let Some(tree_item) =
                                app.file_tree_items.get(app.selected_index)
                            {
                                // Toggle directory expansion/collapse or update diff view
                                if tree_item.is_directory {
                                    app.toggle_directory();
                                } else {
                                    app.update_diff_content();
                                }
                            }
                        }

                        // Left pane switching
                        KeyCode::Char('1') if !app.search_input_mode => app.show_files_pane(),
                        KeyCode::Char('2') if !app.search_input_mode => app.open_history(),

                        // Staging and committing (working tree view)
                        KeyCode::Char('s')
                            if !app.search_input_mode && app.left_pane == LeftPane::Files =>
                        {
                            app.stage_checked_files(true)
                        }
                        KeyCode::Char('S')
                            if !app.search_input_mode && app.left_pane == LeftPane::Files =>
                        {
                            app.stage_checked_files(false)
                        }
                        KeyCode::Char('C')
                            if !app.search_input_mode && app.left_pane == LeftPane::Files =>
                        {
                            app.begin_commit()
                        }

                        // Open the selected file in an editor
                        KeyCode::Char('o')
                            if !app.search_input_mode && app.left_pane == LeftPane::Files =>
                        {
                            open_selected_in_editor(terminal, &mut app)?;
                        }

                        // Comment / question to an agent (side-by-side view)
                        KeyCode::Char('c')
                            if !app.search_input_mode && app.left_pane == LeftPane::Files =>
                        {
                            app.enter_comment_mode()
                        }

                        // Reload diffs
                        KeyCode::Char('r')
                            if !app.search_input_mode && app.left_pane == LeftPane::Files =>
                        {
                            app.reload_diffs()
                        }

                        // Help overlay
                        KeyCode::Char('?') if !app.search_input_mode => {
                            app.show_help = !app.show_help
                        }

                        // Toggle between side-by-side and unified diff view
                        KeyCode::Char('v') if !app.search_input_mode => app.toggle_view_mode(),

                        // Toggle condensed (hunks-only) vs full file view
                        KeyCode::Char('x') if !app.search_input_mode => app.toggle_condensed(),

                        // Toggle flat list vs directory tree
                        KeyCode::Char('t')
                            if !app.search_input_mode && app.left_pane == LeftPane::Files =>
                        {
                            app.toggle_flat_view()
                        }

                        // Jump navigation (disabled only when typing in search)
                        KeyCode::Char('g') if !app.search_input_mode => app.nav_first(),
                        KeyCode::Char('G') if !app.search_input_mode => app.nav_last(),

                        // Vertical scrolling (disabled only when typing in search)
                        KeyCode::Char('d') | KeyCode::PageDown if !app.search_input_mode => {
                            app.scroll_down(10)
                        }
                        KeyCode::Char('u') | KeyCode::PageUp if !app.search_input_mode => {
                            app.scroll_up(10)
                        }
                        KeyCode::Char('f') if !app.search_input_mode => app.scroll_down(20),
                        KeyCode::Char('b') if !app.search_input_mode => app.scroll_up(20),

                        // Shift+arrows move the pane divider
                        KeyCode::Left
                            if key.modifiers.contains(KeyModifiers::SHIFT)
                                && !app.search_input_mode =>
                        {
                            app.resize_file_pane(-5)
                        }
                        KeyCode::Right
                            if key.modifiers.contains(KeyModifiers::SHIFT)
                                && !app.search_input_mode =>
                        {
                            app.resize_file_pane(5)
                        }

                        // Hide/show the file pane (full-width diff)
                        KeyCode::Char('z') if !app.search_input_mode => app.toggle_file_pane(),

                        // Expand/collapse long agent notes
                        KeyCode::Char('n') if !app.search_input_mode => app.toggle_notes_expanded(),

                        // Horizontal scrolling (disabled only when typing in search)
                        KeyCode::Char('h') | KeyCode::Left if !app.search_input_mode => {
                            app.scroll_left(5)
                        }
                        KeyCode::Char('l') | KeyCode::Right if !app.search_input_mode => {
                            app.scroll_right(5)
                        }
                        KeyCode::Char('H') if !app.search_input_mode => app.scroll_left(20),
                        KeyCode::Char('L') if !app.search_input_mode => app.scroll_right(20),

                        // Checkbox toggle (file list only)
                        KeyCode::Char(' ')
                            if !app.search_input_mode && app.left_pane == LeftPane::Files =>
                        {
                            app.toggle_file_checked()
                        }

                        _ => {}
                    }
                }
                Event::Mouse(mouse) => match mouse.kind {
                    MouseEventKind::ScrollDown | MouseEventKind::ScrollUp => {
                        let down = mouse.kind == MouseEventKind::ScrollDown;
                        // Alt+wheel (or Shift+wheel where the terminal forwards
                        // it - Windows Terminal reserves Shift for selection)
                        // scrolls the diff pane horizontally
                        if mouse
                            .modifiers
                            .intersects(KeyModifiers::SHIFT | KeyModifiers::ALT)
                        {
                            if down {
                                app.scroll_right(5);
                            } else {
                                app.scroll_left(5);
                            }
                        } else {
                            app.handle_mouse_scroll(mouse, down);
                        }
                    }
                    // Tilt wheel
                    MouseEventKind::ScrollRight => app.scroll_right(5),
                    MouseEventKind::ScrollLeft => app.scroll_left(5),
                    MouseEventKind::Down(MouseButton::Left) => app.handle_mouse_click(mouse),
                    _ => {}
                },
                _ => {}
            }
        }

        app.auto_refresh_if_due();

        // Agent replies appended to the reply file appear inline
        if app.agent_session.poll_replies() {
            app.refresh_note_display();
        }

        if app.should_quit {
            return Ok(());
        }
    }
}

fn ui(f: &mut Frame, app: &mut App) {
    let has_warning = app.warning_message.is_some();

    // Menu bar on top, content below
    let frame_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(0)])
        .split(f.area());

    render_menu_bar(f, frame_chunks[0], app);

    // Main horizontal split: left list and diff content area. The
    // divider position is user-adjustable (Shift+arrows), z hides the
    // list entirely
    let list_percent = if app.file_pane_hidden {
        0
    } else {
        app.config.file_pane_percent.clamp(10, 60)
    };
    let main_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(list_percent),
            Constraint::Percentage(100 - list_percent),
        ])
        .split(frame_chunks[1]);

    app.regions.left_column = main_chunks[0];

    // Left pane: commit history, or file list with optional search box
    if app.file_pane_hidden {
        app.regions.list_area = Rect::default();
    } else if app.left_pane == LeftPane::History {
        app.regions.list_area = main_chunks[0];
        render_commit_list(f, main_chunks[0], app);
    } else if app.search_mode {
        let left_chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(3), Constraint::Min(0)])
            .split(main_chunks[0]);

        render_search_box(f, left_chunks[0], app);
        app.regions.list_area = left_chunks[1];
        render_file_list(f, left_chunks[1], app);
    } else {
        app.regions.list_area = main_chunks[0];
        render_file_list(f, main_chunks[0], app);
    }

    // Right side vertical split: status line, diff content, and optional warning
    let right_constraints: Vec<Constraint> = if has_warning {
        vec![
            Constraint::Length(3),
            Constraint::Min(0),
            Constraint::Length(3),
        ]
    } else {
        vec![Constraint::Length(3), Constraint::Min(0)]
    };

    let right_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(right_constraints)
        .split(main_chunks[1]);

    render_status_line(f, right_chunks[0], app);
    if app.left_pane == LeftPane::History {
        // History mode: commit graph beside the commit preview
        let history_chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(right_chunks[1]);
        app.regions.graph_area = history_chunks[0];
        render_commit_graph(f, history_chunks[0], app);
        render_diff_content(f, history_chunks[1], app);
    } else {
        app.regions.graph_area = Rect::default();
        if app.view_mode != ViewMode::Unified && app.aligned_rows.is_some() {
            render_side_by_side(f, right_chunks[1], app);
        } else {
            render_diff_content(f, right_chunks[1], app);
        }
    }

    // Render warning bar below diff content if present.
    // Guard on has_warning (not warning_message directly): rendering the
    // diff pane above may set a warning mid-frame, but the layout was
    // already computed without the bar - it will show on the next draw.
    if has_warning {
        render_warning_bar(f, right_chunks[2], app);
    }

    if app.commit_input_mode {
        render_commit_input(f, f.area(), app);
    }

    if app.comment_input_mode {
        render_comment_input(f, f.area(), app);
    }

    if app.show_help {
        render_help_overlay(f, f.area(), app);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    use tssdiff_core::parser::FileDiff;

    #[test]
    fn test_app_new() {
        let config = Config::default();
        let app = App::new(config, vec![], OperationMode::GitWorkingDirectory).unwrap();
        assert!(!app.should_quit);
        assert_eq!(app.selected_index, 0);
        assert_eq!(app.vertical_scroll, 0);
        assert_eq!(app.horizontal_scroll, 0);
    }

    #[test]
    fn test_ui_layout() {
        let backend = TestBackend::new(100, 50);
        let mut terminal = Terminal::new(backend).unwrap();
        let config = Config::default();
        let mut app = App::new(config, vec![], OperationMode::GitWorkingDirectory).unwrap();

        terminal.draw(|f| ui(f, &mut app)).unwrap();

        let buffer = terminal.backend().buffer();
        assert!(buffer.area().width == 100);
        assert!(buffer.area().height == 50);
    }

    #[test]
    fn test_render_file_list() {
        let backend = TestBackend::new(40, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        let config = Config::default();
        let file_diffs = vec![
            FileDiff {
                filename: "test1.rs".to_string(),
                old_path: None,
                new_path: None,
                content: "test content".to_string(),
                added_lines: 1,
                removed_lines: 0,
                diff_key: None,
            },
            FileDiff {
                filename: "test2.rs".to_string(),
                old_path: None,
                new_path: None,
                content: "test content 2".to_string(),
                added_lines: 0,
                removed_lines: 1,
                diff_key: None,
            },
        ];
        let mut app = App::new(config, file_diffs, OperationMode::GitWorkingDirectory).unwrap();

        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 40, 20);
                render_file_list(f, area, &mut app);
            })
            .unwrap();

        let buffer = terminal.backend().buffer();
        let content = buffer_to_string(buffer);
        assert!(content.contains("Files & Directories"));
        assert!(content.contains("test1.rs"));
        assert!(content.contains("test2.rs"));
    }

    #[test]
    fn test_render_diff_content() {
        let backend = TestBackend::new(60, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        let config = Config::default();
        let mut app = App::new(config, vec![], OperationMode::GitWorkingDirectory).unwrap();

        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 60, 20);
                render_diff_content(f, area, &mut app);
            })
            .unwrap();

        let buffer = terminal.backend().buffer();
        let content = buffer_to_string(buffer);
        assert!(content.contains("Diff Content"));
        assert!(content.contains("No diff content available"));
    }

    #[test]
    fn test_render_warning_bar() {
        let backend = TestBackend::new(100, 50);
        let mut terminal = Terminal::new(backend).unwrap();
        let config = Config::default();
        let mut app = App::new(config, vec![], OperationMode::GitWorkingDirectory).unwrap();
        app.warning_message = Some("Failed to process with diff tool: program not found".into());

        terminal.draw(|f| ui(f, &mut app)).unwrap();

        let buffer = terminal.backend().buffer();
        let content = buffer_to_string(buffer);
        assert!(content.contains("Warning"));
        assert!(content.contains("Failed to process with diff tool"));
    }

    #[test]
    fn test_render_side_by_side_panes() {
        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let config = Config::default();
        let file_diffs = vec![FileDiff {
            filename: "test.rs".to_string(),
            old_path: None,
            new_path: None,
            content: "-old line\n+new line\n same\n".to_string(),
            added_lines: 1,
            removed_lines: 1,
            diff_key: None,
        }];
        let mut app = App::new(
            config,
            file_diffs,
            OperationMode::Compare {
                target1: "a".to_string(),
                target2: "b".to_string(),
            },
        )
        .unwrap();
        app.aligned_rows = Some(side_by_side::align("old line\nsame\n", "new line\nsame\n"));

        terminal.draw(|f| ui(f, &mut app)).unwrap();

        let content = buffer_to_string(terminal.backend().buffer());
        assert!(content.contains("Before"));
        assert!(content.contains("After"));
        assert!(content.contains("old line"));
        assert!(content.contains("new line"));
        assert!(content.contains("same"));
    }

    #[test]
    fn test_render_condensed_gap() {
        let backend = TestBackend::new(120, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        let config = Config::default();
        let mut app = App::new(
            config,
            vec![],
            OperationMode::Compare {
                target1: "a".to_string(),
                target2: "b".to_string(),
            },
        )
        .unwrap();

        // 30 identical lines with one change at the end
        let old: String = (1..=30).map(|i| format!("line{i}\n")).collect();
        let new = old.replace("line30\n", "changed30\n");
        let rows = side_by_side::align(&old, &new);
        app.display_rows = side_by_side::condense(&rows, 3);
        app.aligned_rows = Some(rows);

        terminal.draw(|f| ui(f, &mut app)).unwrap();

        let content = buffer_to_string(terminal.backend().buffer());
        assert!(content.contains("lines hidden"), "got: {content}");
        assert!(content.contains("changed30"));
        // Collapsed lines from the top of the file are not rendered
        assert!(!content.contains("line1 "));
    }

    #[test]
    fn test_render_menu_bar_and_click() {
        use crossterm::event::KeyModifiers;

        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let config = Config::default();
        let mut app = App::new(config, vec![], OperationMode::GitWorkingDirectory).unwrap();

        terminal.draw(|f| ui(f, &mut app)).unwrap();

        let content = buffer_to_string(terminal.backend().buffer());
        assert!(content.contains("tssdiff"));
        assert!(content.contains("Files"));
        assert!(content.contains("History"));
        assert!(content.contains("Help"));
        assert_eq!(app.regions.menu_items.len(), 5);

        // Clicking the Help menu entry opens the help overlay
        let (rect, _) = *app
            .regions
            .menu_items
            .iter()
            .find(|(_, action)| *action == MenuAction::Help)
            .unwrap();
        app.handle_mouse_click(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: rect.x,
            row: rect.y,
            modifiers: KeyModifiers::empty(),
        });
        assert!(app.show_help);

        terminal.draw(|f| ui(f, &mut app)).unwrap();
        let content = buffer_to_string(terminal.backend().buffer());
        assert!(content.contains("Navigation"));
        assert!(content.contains("side-by-side"));
    }

    #[test]
    fn test_render_history_pane() {
        // Wide enough that the 20% left pane fits the full title and subject
        let backend = TestBackend::new(220, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let config = Config::default();
        let mut app = App::new(config, vec![], OperationMode::GitWorkingDirectory).unwrap();
        app.commits = vec![CommitInfo {
            hash: "abc1234".to_string(),
            date: "2026-07-06 10:00".to_string(),
            subject: "test commit subject".to_string(),
        }];
        app.graph_rows = vec![GraphRow {
            graph: "* ".to_string(),
            hash: Some("abc1234".to_string()),
            refs: "(HEAD -> main)".to_string(),
            subject: "test commit subject".to_string(),
        }];
        app.left_pane = LeftPane::History;
        app.commit_index = 1;
        app.commit_list_state.select(Some(1));
        app.diff_output = "commit preview".to_string();
        app.aligned_rows = None;

        terminal.draw(|f| ui(f, &mut app)).unwrap();

        let content = buffer_to_string(terminal.backend().buffer());
        assert!(content.contains("History (1 commits)"));
        assert!(content.contains("Working tree"));
        assert!(content.contains("abc1234"));
        assert!(content.contains("test commit subject"));
        // Status line shows the selected commit's date
        assert!(content.contains("2026-07-06 10:00"));
        // Graph pane renders beside the preview
        assert!(content.contains("Graph"));
        assert!(content.contains("(HEAD -> main)"));
        assert!(content.contains("commit preview"));
    }

    #[test]
    fn test_warning_set_mid_frame_does_not_panic() {
        let backend = TestBackend::new(90, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let config = Config {
            git: config::GitConfig {
                paging: config::GitPagingConfig {
                    pager: "nonexistent_diff_tool_xyz".to_string(),
                    ..Default::default()
                },
                ..Default::default()
            },
            ..Default::default()
        };

        let file_diffs = vec![FileDiff {
            filename: "test.rs".to_string(),
            old_path: None,
            new_path: None,
            content: "+added".to_string(),
            added_lines: 1,
            removed_lines: 0,
            diff_key: None,
        }];
        let mut app = App::new(
            config,
            file_diffs,
            OperationMode::Compare {
                target1: "a".to_string(),
                target2: "b".to_string(),
            },
        )
        .unwrap();
        // External diff tools only run in the unified view
        app.view_mode = ViewMode::Unified;

        // First frame lays out without a warning bar, then the failing
        // pager sets the warning while the diff pane renders - this must
        // not panic even though the bar's area was never allocated
        terminal.draw(|f| ui(f, &mut app)).unwrap();
        assert!(app.warning_message.is_some());

        // The warning bar appears on the next frame
        terminal.draw(|f| ui(f, &mut app)).unwrap();
        let content = buffer_to_string(terminal.backend().buffer());
        assert!(content.contains("Warning"));
    }

    /// App with one file selected and side-by-side rows prepared, and the
    /// agent session pointed at a temp dir so tests never touch the repo
    fn app_with_rows(temp: &std::path::Path) -> App {
        let config = Config::default();
        let file_diffs = vec![FileDiff {
            filename: "test.rs".to_string(),
            old_path: None,
            new_path: None,
            content: "-old\n+new\n".to_string(),
            added_lines: 1,
            removed_lines: 1,
            diff_key: None,
        }];
        let mut app = App::new(
            config,
            file_diffs,
            OperationMode::Compare {
                target1: "a".to_string(),
                target2: "b".to_string(),
            },
        )
        .unwrap();
        app.agent_session = AgentSession::new(temp.to_path_buf());
        let rows = side_by_side::align("a\nold\nz\n", "a\nnew\nz\n");
        app.display_rows = (0..rows.len()).map(side_by_side::DisplayRow::Row).collect();
        app.aligned_rows = Some(rows);
        app
    }

    #[test]
    fn test_comment_mode_send_via_file_sink() {
        let temp = tempfile::tempdir().unwrap();
        let mut app = app_with_rows(temp.path());
        app.config.agent.sink = config::SinkKind::File;

        app.enter_comment_mode();
        assert_eq!(app.comment_cursor, Some(0));
        // Cursor moves over rows and clamps at the end
        app.comment_cursor_move(1, 1);
        assert_eq!(app.comment_cursor, Some(1));
        app.comment_cursor_move(1, 10);
        assert_eq!(app.comment_cursor, Some(2));

        app.comment_cursor = Some(1); // the changed row
        app.comment_text = "why this change?".to_string();
        app.comment_kind = FeedbackKind::Question;
        app.send_comment();

        // Payload written through the file sink
        let outbox = temp.path().join(".tssdiff").join("outbox.jsonl");
        let content = std::fs::read_to_string(outbox).unwrap();
        let payload: serde_json::Value = serde_json::from_str(content.trim()).unwrap();
        assert_eq!(payload["kind"], "question");
        assert_eq!(payload["file"], "test.rs");
        assert_eq!(payload["new_line"], 2);
        assert!(payload["hunk_text"].as_str().unwrap().contains("+ new"));

        // Question resets the reply transport for this session
        assert!(temp.path().join(".tssdiff").join("replies.jsonl").exists());

        // Comment mode closed, own note spliced beneath the row
        assert!(app.comment_cursor.is_none());
        assert!(!app.comment_input_mode);
        assert!(
            app.display_rows
                .iter()
                .any(|entry| matches!(entry, side_by_side::DisplayRow::Note { note: 0, line: 0 }))
        );
        assert!(app.warning_message.as_deref().unwrap().contains("sent"));
    }

    #[test]
    fn test_range_selection_sends_spans() {
        let temp = tempfile::tempdir().unwrap();
        let mut app = app_with_rows(temp.path());
        app.config.agent.sink = config::SinkKind::File;

        app.enter_comment_mode();
        // Anchor on the first row, extend to the last
        app.toggle_comment_anchor();
        app.comment_cursor_move(1, 2);
        assert_eq!(app.comment_selection(), Some((0, 2)));

        app.comment_text = "whole block".to_string();
        app.send_comment();

        let outbox = temp.path().join(".tssdiff").join("outbox.jsonl");
        let content = std::fs::read_to_string(outbox).unwrap();
        let payload: serde_json::Value = serde_json::from_str(content.trim()).unwrap();
        assert_eq!(payload["new_line"], 1);
        assert_eq!(payload["new_range"][0], 1);
        assert_eq!(payload["new_range"][1], 3);
        assert_eq!(payload["old_range"][1], 3);
        assert!(app.comment_anchor.is_none());
    }

    #[test]
    fn test_agent_note_renders_inline() {
        let temp = tempfile::tempdir().unwrap();
        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = app_with_rows(temp.path());

        app.agent_session.notes.push(tssdiff_core::agent::Note {
            reply_to: None,
            file: "test.rs".to_string(),
            old_line: None,
            new_line: Some(2),
            body: "This renames the variable.".to_string(),
            author: "agent".to_string(),
        });
        app.refresh_note_display();

        terminal.draw(|f| ui(f, &mut app)).unwrap();
        let content = buffer_to_string(terminal.backend().buffer());
        assert!(content.contains("agent:"), "got: {content}");
        assert!(content.contains("This renames the variable."));
    }

    #[test]
    fn test_after_only_view_renders_single_pane() {
        let temp = tempfile::tempdir().unwrap();
        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = app_with_rows(temp.path());
        app.view_mode = ViewMode::AfterOnly;

        terminal.draw(|f| ui(f, &mut app)).unwrap();
        let content = buffer_to_string(terminal.backend().buffer());
        assert!(content.contains("After (full width)"));
        assert!(content.contains("new"));
        assert!(!content.contains("Before"));
        // The old side's text does not appear anywhere
        assert!(!content.contains("old"));
    }

    #[test]
    fn test_view_mode_cycles_three_states() {
        let temp = tempfile::tempdir().unwrap();
        let mut app = app_with_rows(temp.path());
        assert_eq!(app.view_mode, ViewMode::SideBySide);
        app.toggle_view_mode();
        assert_eq!(app.view_mode, ViewMode::AfterOnly);
        app.toggle_view_mode();
        assert_eq!(app.view_mode, ViewMode::Unified);
        app.toggle_view_mode();
        assert_eq!(app.view_mode, ViewMode::SideBySide);
    }

    #[test]
    fn test_long_note_wraps_and_folds() {
        let temp = tempfile::tempdir().unwrap();
        let mut app = app_with_rows(temp.path());
        app.last_note_wrap_width = 40;

        // A long reply: many short lines force wrapping past the fold
        let body = (1..=12)
            .map(|i| format!("answer line number {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        app.agent_session.notes.push(tssdiff_core::agent::Note {
            reply_to: None,
            file: "test.rs".to_string(),
            old_line: None,
            new_line: Some(2),
            body,
            author: "agent".to_string(),
        });
        app.refresh_note_display();

        // Folded: 4 shown lines + the expander
        let folded = &app.wrapped_notes[0];
        assert_eq!(folded.len(), 5);
        assert!(folded.last().unwrap().contains("n: expand"));
        let note_rows = app
            .display_rows
            .iter()
            .filter(|entry| matches!(entry, side_by_side::DisplayRow::Note { .. }))
            .count();
        assert_eq!(note_rows, 5);

        // Expanded: everything visible
        app.toggle_notes_expanded();
        assert!(app.wrapped_notes[0].len() >= 12);
        assert!(!app.wrapped_notes[0].iter().any(|l| l.contains("n: expand")));
    }

    #[test]
    fn test_comment_input_popup_renders() {
        let temp = tempfile::tempdir().unwrap();
        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = app_with_rows(temp.path());
        app.comment_cursor = Some(1);
        app.comment_input_mode = true;
        app.comment_text = "typed text".to_string();
        app.comment_kind = FeedbackKind::Question;

        terminal.draw(|f| ui(f, &mut app)).unwrap();
        let content = buffer_to_string(terminal.backend().buffer());
        assert!(content.contains("Send to agent [Question]"));
        assert!(content.contains("typed text"));
    }

    fn buffer_to_string(buffer: &Buffer) -> String {
        let mut result = String::new();
        for y in 0..buffer.area().height {
            for x in 0..buffer.area().width {
                let cell = buffer.cell((x, y)).unwrap();
                result.push_str(cell.symbol());
            }
            result.push('\n');
        }
        result
    }
}
