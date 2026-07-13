//! Agent feedback: push review comments/questions from the diff view to
//! an AI coding agent, and render its replies inline.
//!
//! Outbound: a neutral JSON payload goes to a configurable sink
//! (clipboard / file / command). tssdiff knows nothing about any
//! specific agent harness - a command sink adapter bridges to one.
//!
//! Inbound: agents append JSON lines to `.tssdiff/replies.jsonl` in the
//! repository; tssdiff polls it and shows entries next to the code.
//! Session-scoped: entries existing before startup are skipped, and the
//! file is truncated on the first send of a session.

use crate::config::{AgentConfig, SinkKind};
use crate::side_by_side::{AlignedRow, RowKind};
use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use std::io::{BufRead, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Schema version of the outbound batch envelope
const PAYLOAD_VERSION: u32 = 2;
/// Lines of unchanged context around the selected line in the excerpt
const EXCERPT_CONTEXT: usize = 3;
/// Hard cap on excerpt size so a huge hunk cannot flood the payload
const EXCERPT_MAX_LINES: usize = 60;
/// Minimum interval between reply-file polls
const POLL_INTERVAL: Duration = Duration::from_millis(800);

/// What the user is sending: a remark, or a question expecting a reply
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeedbackKind {
    Comment,
    Question,
}

impl FeedbackKind {
    pub fn toggle(self) -> Self {
        match self {
            FeedbackKind::Comment => FeedbackKind::Question,
            FeedbackKind::Question => FeedbackKind::Comment,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            FeedbackKind::Comment => "Comment",
            FeedbackKind::Question => "Question",
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            FeedbackKind::Comment => "comment",
            FeedbackKind::Question => "question",
        }
    }
}

/// One reviewed item inside a batch: a comment or question anchored to a
/// line range of a single file. Adapters receive these as the `items`
/// array of a `FeedbackBatch`.
#[derive(Debug, Clone, Serialize)]
pub struct FeedbackItem {
    pub id: String,
    pub kind: String,
    /// Repository-relative path of the file under review
    pub file: String,
    pub old_line: Option<usize>,
    pub new_line: Option<usize>,
    /// Inclusive [start, end] line spans when a multi-line range was
    /// selected (single-line selections have start == end)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old_range: Option<[usize; 2]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new_range: Option<[usize; 2]>,
    /// Unified-style excerpt of the change around the selected lines
    pub hunk_text: String,
    pub comment: String,
}

/// Neutral outbound envelope (schema v2): one or more reviewed items
/// flushed together as a single JSON document. The schema is the public
/// contract for command-sink adapters.
#[derive(Debug, Serialize)]
pub struct FeedbackBatch {
    pub version: u32,
    /// Absolute repository root (or invocation directory outside a repo)
    pub repo: String,
    /// Absolute path agents should append reply JSON lines to
    pub reply_file: String,
    /// Unix epoch seconds at flush time
    pub timestamp: u64,
    pub items: Vec<FeedbackItem>,
}

impl FeedbackBatch {
    fn has_question(&self) -> bool {
        self.items.iter().any(|item| item.kind == "question")
    }
}

/// One inline note: either a reply read from the reply file, or the
/// user's own sent feedback (shown immediately as a sent marker)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Note {
    #[serde(default)]
    pub reply_to: Option<String>,
    pub file: String,
    #[serde(default)]
    pub old_line: Option<usize>,
    #[serde(default)]
    pub new_line: Option<usize>,
    pub body: String,
    #[serde(default = "default_author")]
    pub author: String,
    /// Local-only: a staged draft not yet flushed. Never (de)serialized -
    /// replies read from the file are always already "sent".
    #[serde(skip)]
    pub pending: bool,
}

fn default_author() -> String {
    "agent".to_string()
}

impl Note {
    /// Does this note anchor to the given aligned row?
    pub fn anchors_to(&self, row: &AlignedRow) -> bool {
        if let Some(new_line) = self.new_line {
            return row.new.as_ref().is_some_and(|(n, _)| *n == new_line);
        }
        if let Some(old_line) = self.old_line {
            return row.old.as_ref().is_some_and(|(n, _)| *n == old_line);
        }
        false
    }
}

/// Per-session agent state: outbound id sequence and the inbound
/// reply-file cursor. Notes accumulate in memory; the file is only a
/// transport and gets truncated on the first send of each session.
pub struct AgentSession {
    repo: PathBuf,
    reply_path: PathBuf,
    read_offset: u64,
    sent_this_session: bool,
    next_id: u32,
    last_poll: Instant,
    pub notes: Vec<Note>,
    /// Drafts staged locally, flushed together on the next `flush`
    pub pending: Vec<FeedbackItem>,
}

impl AgentSession {
    pub fn new(repo: PathBuf) -> Self {
        let reply_path = repo.join(".tssdiff").join("replies.jsonl");
        // Skip anything a previous session (or a late reply) left behind
        let read_offset = std::fs::metadata(&reply_path).map(|m| m.len()).unwrap_or(0);
        Self {
            repo,
            reply_path,
            read_offset,
            sent_this_session: false,
            next_id: 1,
            last_poll: Instant::now(),
            notes: Vec::new(),
            pending: Vec::new(),
        }
    }

    fn next_feedback_id(&mut self) -> String {
        let id = format!("fb-{}-{}", unix_now(), self.next_id);
        self.next_id += 1;
        id
    }

    /// Build a feedback item for the selected row range (inclusive row
    /// indices into `rows`; single-line selections have start == end)
    fn build_item(
        &mut self,
        kind: FeedbackKind,
        file: &str,
        rows: &[AlignedRow],
        selection_start: usize,
        selection_end: usize,
        comment: &str,
    ) -> FeedbackItem {
        let selection_start = selection_start.min(rows.len().saturating_sub(1));
        let selection_end = selection_end.clamp(selection_start, rows.len().saturating_sub(1));
        let selected = &rows[selection_start..=selection_end];

        let old_lines: Vec<usize> = selected
            .iter()
            .filter_map(|row| row.old.as_ref().map(|(n, _)| *n))
            .collect();
        let new_lines: Vec<usize> = selected
            .iter()
            .filter_map(|row| row.new.as_ref().map(|(n, _)| *n))
            .collect();
        let span =
            |lines: &[usize]| -> Option<[usize; 2]> { Some([*lines.first()?, *lines.last()?]) };

        FeedbackItem {
            id: self.next_feedback_id(),
            kind: kind.as_str().to_string(),
            file: file.replace('\\', "/"),
            old_line: old_lines.first().copied(),
            new_line: new_lines.first().copied(),
            old_range: span(&old_lines),
            new_range: span(&new_lines),
            hunk_text: excerpt(rows, selection_start, selection_end),
            comment: comment.to_string(),
        }
    }

    /// Stage a comment/question as an un-sent draft. It shows immediately
    /// as a pending inline note; nothing is transmitted until `flush`.
    /// Returns the assigned correlation id.
    pub fn stage(
        &mut self,
        kind: FeedbackKind,
        file: &str,
        rows: &[AlignedRow],
        selection_start: usize,
        selection_end: usize,
        comment: &str,
    ) -> String {
        let item = self.build_item(kind, file, rows, selection_start, selection_end, comment);
        let id = item.id.clone();
        self.notes.push(Note {
            reply_to: Some(item.id.clone()),
            file: item.file.clone(),
            old_line: item.old_line,
            new_line: item.new_line,
            body: item.comment.clone(),
            author: "you".to_string(),
            pending: true,
        });
        self.pending.push(item);
        id
    }

    /// How many drafts are currently staged
    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }

    /// Discard every staged draft (and its inline echo note). Returns how
    /// many were dropped.
    pub fn discard_pending(&mut self) -> usize {
        let dropped = self.pending.len();
        self.pending.clear();
        self.notes.retain(|note| !note.pending);
        dropped
    }

    /// Flush every staged draft as ONE batch through the configured sink.
    /// On success the drafts turn into sent inline notes and a short
    /// status string is returned. On failure the drafts are kept so no
    /// text is lost. Errors (rather than silently no-ops) when nothing is
    /// staged.
    pub fn flush(&mut self, config: &AgentConfig) -> Result<String> {
        if self.pending.is_empty() {
            return Err(anyhow!("No staged feedback to send"));
        }
        let batch = FeedbackBatch {
            version: PAYLOAD_VERSION,
            repo: path_string(&self.repo),
            reply_file: path_string(&self.reply_path),
            timestamp: unix_now(),
            items: std::mem::take(&mut self.pending),
        };

        let result = match config.sink {
            SinkKind::Clipboard => send_clipboard(&format_markdown_batch(&batch))
                .map(|()| "copied to clipboard".to_string()),
            SinkKind::File => self
                .prepare_outbox(config)
                .and_then(|path| append_json_line(&path, &batch).map(|()| path))
                .map(|path| format!("appended to {}", path.display())),
            SinkKind::Command => self
                .prepare_session_dir()
                .and_then(|_| send_command(config, &batch)),
        };

        let status = match result {
            Ok(status) => status,
            Err(e) => {
                // Put the drafts back so the user can retry without retyping
                self.pending = batch.items;
                return Err(e);
            }
        };

        // Questions expect replies: reset the transport file so this
        // session only ever sees its own conversation
        if batch.has_question() {
            self.reset_reply_file_once()?;
        }

        let count = batch.items.len();
        // The whole queue was flushed: every staged echo is now sent
        for note in &mut self.notes {
            note.pending = false;
        }
        Ok(format!("{count} item(s): {status}"))
    }

    /// Poll the reply file; returns true when new notes arrived
    pub fn poll_replies(&mut self) -> bool {
        if self.last_poll.elapsed() < POLL_INTERVAL {
            return false;
        }
        self.last_poll = Instant::now();

        let Ok(metadata) = std::fs::metadata(&self.reply_path) else {
            return false;
        };
        let len = metadata.len();
        if len < self.read_offset {
            // Truncated externally: start over from the top
            self.read_offset = 0;
        }
        if len == self.read_offset {
            return false;
        }

        let Ok(file) = std::fs::File::open(&self.reply_path) else {
            return false;
        };
        let mut reader = std::io::BufReader::new(file);
        if reader.seek(SeekFrom::Start(self.read_offset)).is_err() {
            return false;
        }

        let mut added = false;
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) => break,
                Ok(bytes) => {
                    if !line.ends_with('\n') {
                        // Partial line still being written: retry next poll
                        break;
                    }
                    self.read_offset += bytes as u64;
                    let trimmed = line.trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                    if let Ok(mut note) = serde_json::from_str::<Note>(trimmed) {
                        note.file = note.file.replace('\\', "/");
                        self.notes.push(note);
                        added = true;
                    }
                }
                Err(_) => break,
            }
        }
        added
    }

    /// Create `.tssdiff/` (self-ignoring via its own .gitignore) and, once
    /// per session, truncate the reply transport file
    fn prepare_session_dir(&mut self) -> Result<PathBuf> {
        let dir = self.repo.join(".tssdiff");
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("Failed to create {}", dir.display()))?;
        let gitignore = dir.join(".gitignore");
        if !gitignore.exists() {
            let _ = std::fs::write(&gitignore, "*\n");
        }
        Ok(dir)
    }

    fn reset_reply_file_once(&mut self) -> Result<()> {
        if self.sent_this_session {
            return Ok(());
        }
        self.prepare_session_dir()?;
        std::fs::write(&self.reply_path, "")
            .with_context(|| format!("Failed to reset {}", self.reply_path.display()))?;
        self.read_offset = 0;
        self.sent_this_session = true;
        Ok(())
    }

    fn prepare_outbox(&mut self, config: &AgentConfig) -> Result<PathBuf> {
        if !config.outbox_file.trim().is_empty() {
            return Ok(PathBuf::from(config.outbox_file.trim()));
        }
        Ok(self.prepare_session_dir()?.join("outbox.jsonl"))
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn path_string(path: &Path) -> String {
    path.display().to_string().replace('\\', "/")
}

fn append_json_line<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create {}", parent.display()))?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("Failed to open {}", path.display()))?;
    let json = serde_json::to_string(value)?;
    writeln!(file, "{json}")?;
    Ok(())
}

fn send_clipboard(text: &str) -> Result<()> {
    let mut clipboard =
        arboard::Clipboard::new().map_err(|e| anyhow!("Clipboard unavailable: {e}"))?;
    clipboard
        .set_text(text.to_string())
        .map_err(|e| anyhow!("Clipboard write failed: {e}"))?;
    Ok(())
}

/// Command sink contract: spawn the configured command, write the JSON
/// payload to stdin, wait up to the timeout. Exit 0 means delivered;
/// anything else surfaces stdout/stderr as the error message.
fn send_command(config: &AgentConfig, batch: &FeedbackBatch) -> Result<String> {
    use std::process::{Command, Stdio};

    let command_str = config.sink_command.trim();
    if command_str.is_empty() {
        return Err(anyhow!(
            "agent.sinkCommand is not configured (set it in config.yaml)"
        ));
    }
    let mut parts = command_str.split_whitespace();
    let program = parts.next().unwrap();

    // The sink contract is non-interactive; on Windows keep the spawn
    // from flashing a console when the caller is a GUI-subsystem app
    #[allow(unused_mut)]
    let mut command = Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    let mut child = command
        .args(parts)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| anyhow!("Failed to spawn {program}: {e}"))?;

    if let Some(mut stdin) = child.stdin.take() {
        let json = serde_json::to_string(batch)?;
        stdin
            .write_all(json.as_bytes())
            .map_err(|e| anyhow!("Failed to write payload to {program}: {e}"))?;
    }

    let timeout = Duration::from_millis(config.sink_timeout_ms.max(100));
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(anyhow!(
                        "{program} timed out after {}ms",
                        config.sink_timeout_ms
                    ));
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(e) => return Err(anyhow!("Failed to wait for {program}: {e}")),
        }
    };

    let mut output = String::new();
    if let Some(mut stdout) = child.stdout.take() {
        let _ = std::io::Read::read_to_string(&mut stdout, &mut output);
    }
    if status.success() {
        let first_line = output.lines().next().unwrap_or("delivered").trim();
        Ok(format!("sent via {program}: {first_line}"))
    } else {
        let mut stderr_text = String::new();
        if let Some(mut stderr) = child.stderr.take() {
            let _ = std::io::Read::read_to_string(&mut stderr, &mut stderr_text);
        }
        let detail = stderr_text.lines().next().unwrap_or("").trim().to_string();
        let detail = if detail.is_empty() {
            output
                .lines()
                .next()
                .unwrap_or("no output")
                .trim()
                .to_string()
        } else {
            detail
        };
        Err(anyhow!("{program} failed ({}): {detail}", status))
    }
}

/// Location label for one item: `file:line`, or `file:line (old)` for a
/// deleted line with no new-side anchor.
fn item_location(item: &FeedbackItem) -> String {
    let mut location = item.file.clone();
    if let Some(new_line) = item.new_line {
        location.push_str(&format!(":{new_line}"));
    } else if let Some(old_line) = item.old_line {
        location.push_str(&format!(":{old_line} (old)"));
    }
    location
}

/// Markdown rendering used when tssdiff itself is the bridge (clipboard
/// sink): human-pasteable, listing every item in the batch, with a
/// single machine-followable reply instruction covering all questions.
pub fn format_markdown_batch(batch: &FeedbackBatch) -> String {
    let n = batch.items.len();
    let mut text = format!(
        "# Diff review \u{2014} {n} item{}\n\nRepository: `{}`\n",
        if n == 1 { "" } else { "s" },
        batch.repo,
    );

    for (index, item) in batch.items.iter().enumerate() {
        let location = item_location(item);
        let kind_label = if item.kind == "question" {
            "Question"
        } else {
            "Comment"
        };
        text.push_str(&format!(
            "\n## {}. {location} \u{b7} {kind_label}\n\n```diff\n{}\n```\n\n{}\n",
            index + 1,
            item.hunk_text.trim_end(),
            item.comment,
        ));
    }

    let questions: Vec<&FeedbackItem> = batch
        .items
        .iter()
        .filter(|item| item.kind == "question")
        .collect();
    if !questions.is_empty() {
        text.push_str(&format!(
            "\n---\nPlease answer the question(s) above by appending ONE line of JSON per \
             answer to `{}` (create the file if needed, one line each, do not edit other \
             lines):\n",
            batch.reply_file
        ));
        for item in questions {
            let reply = serde_json::json!({
                "reply_to": item.id,
                "file": item.file,
                "new_line": item.new_line,
                "old_line": item.old_line,
                "body": "<your answer>",
                "author": "<your name>",
            });
            text.push_str(&format!("{reply}\n"));
        }
    }
    text
}

/// Wrap note text to display widths: the first output line has its own
/// budget (the author prefix takes room), continuations another. Breaks
/// at the last space inside the window when there is one, hard-breaks
/// otherwise (CJK text has no spaces).
pub fn wrap_body(text: &str, first_width: usize, rest_width: usize) -> Vec<String> {
    use unicode_width::UnicodeWidthChar;

    let mut out: Vec<String> = Vec::new();
    for raw in text.lines() {
        let mut budget = if out.is_empty() {
            first_width
        } else {
            rest_width
        }
        .max(4);
        if raw.is_empty() {
            out.push(String::new());
            continue;
        }
        let mut current = String::new();
        let mut width = 0usize;
        let mut last_space: Option<usize> = None; // byte offset into current
        for ch in raw.chars() {
            let char_width = ch.width().unwrap_or(0);
            if width + char_width > budget && !current.is_empty() {
                if let Some(space) = last_space {
                    let tail = current[space..].trim_start().to_string();
                    current.truncate(space);
                    out.push(std::mem::take(&mut current));
                    width = tail.chars().map(|c| c.width().unwrap_or(0)).sum();
                    current = tail;
                } else {
                    out.push(std::mem::take(&mut current));
                    width = 0;
                }
                last_space = None;
                budget = rest_width.max(4);
            }
            if ch == ' ' {
                last_space = Some(current.len());
            }
            current.push(ch);
            width += char_width;
        }
        out.push(current);
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
}

/// Unified-style excerpt around the selected row range: the contiguous
/// changed block containing it, plus a few context lines on each side.
/// Selected rows are marked with a leading `>`
pub fn excerpt(rows: &[AlignedRow], selection_start: usize, selection_end: usize) -> String {
    if rows.is_empty() {
        return String::new();
    }
    let selection_start = selection_start.min(rows.len() - 1);
    let selection_end = selection_end.clamp(selection_start, rows.len() - 1);

    // Expand across the contiguous non-context block around the selection
    let mut start = selection_start;
    while start > 0 && rows[start - 1].kind != RowKind::Context {
        start -= 1;
    }
    let mut end = selection_end;
    while end + 1 < rows.len() && rows[end + 1].kind != RowKind::Context {
        end += 1;
    }
    let start = start.saturating_sub(EXCERPT_CONTEXT);
    let end = (end + EXCERPT_CONTEXT).min(rows.len() - 1);

    // Keep the selection start inside the window when the cap trims it
    let (start, end) = if end - start + 1 > EXCERPT_MAX_LINES {
        let s = selection_start.saturating_sub(EXCERPT_CONTEXT).max(start);
        (s, (s + EXCERPT_MAX_LINES - 1).min(end))
    } else {
        (start, end)
    };

    let mut lines = Vec::new();
    for (index, row) in rows[start..=end].iter().enumerate() {
        let absolute = start + index;
        let marker = if (selection_start..=selection_end).contains(&absolute) {
            ">"
        } else {
            " "
        };
        match row.kind {
            RowKind::Context => {
                if let Some((_, text)) = &row.new {
                    lines.push(format!("{marker}  {text}"));
                }
            }
            RowKind::Removed => {
                if let Some((_, text)) = &row.old {
                    lines.push(format!("{marker}- {text}"));
                }
            }
            RowKind::Added => {
                if let Some((_, text)) = &row.new {
                    lines.push(format!("{marker}+ {text}"));
                }
            }
            RowKind::Modified => {
                if let Some((_, text)) = &row.old {
                    lines.push(format!("{marker}- {text}"));
                }
                if let Some((_, text)) = &row.new {
                    lines.push(format!("{marker}+ {text}"));
                }
            }
        }
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::side_by_side::align;

    fn session(dir: &Path) -> AgentSession {
        AgentSession::new(dir.to_path_buf())
    }

    #[test]
    fn test_item_fields_and_batch_schema() {
        let rows = align("a\nb\nc\n", "a\nB\nc\n");
        let temp = tempfile::tempdir().unwrap();
        let mut s = session(temp.path());
        s.stage(FeedbackKind::Question, "src\\lib.rs", &rows, 1, 1, "why?");

        // Staging populates the draft queue and echoes a pending note
        assert_eq!(s.pending.len(), 1);
        let item = &s.pending[0];
        assert_eq!(item.kind, "question");
        assert_eq!(item.file, "src/lib.rs");
        assert_eq!(item.old_line, Some(2));
        assert_eq!(item.new_line, Some(2));
        assert_eq!(item.old_range, Some([2, 2]));
        assert_eq!(item.new_range, Some([2, 2]));
        assert!(item.hunk_text.contains("- b"));
        assert!(item.hunk_text.contains("+ B"));
        assert_eq!(s.notes.len(), 1);
        assert!(s.notes[0].pending);

        // Flush to the file sink and inspect the v2 envelope
        let config = AgentConfig {
            sink: SinkKind::File,
            ..Default::default()
        };
        s.flush(&config).unwrap();
        let outbox = temp.path().join(".tssdiff").join("outbox.jsonl");
        let content = std::fs::read_to_string(outbox).unwrap();
        let batch: serde_json::Value = serde_json::from_str(content.trim()).unwrap();
        assert_eq!(batch["version"], 2);
        assert!(batch["repo"].is_string());
        assert!(
            batch["reply_file"]
                .as_str()
                .unwrap()
                .ends_with(".tssdiff/replies.jsonl")
        );
        let items = batch["items"].as_array().unwrap();
        assert_eq!(items.len(), 1);
        for key in [
            "id",
            "kind",
            "file",
            "old_line",
            "new_line",
            "hunk_text",
            "comment",
        ] {
            assert!(items[0].get(key).is_some(), "missing {key}");
        }

        // Flushed: queue cleared and the echo note is now sent
        assert!(s.pending.is_empty());
        assert!(!s.notes[0].pending);
    }

    #[test]
    fn test_excerpt_marks_target_and_caps() {
        let old: String = (1..=100).map(|i| format!("line{i}\n")).collect();
        let new = old.replace("line50\n", "changed50\n");
        let rows = align(&old, &new);
        let target = rows
            .iter()
            .position(|r| r.kind != RowKind::Context)
            .unwrap();

        let text = excerpt(&rows, target, target);
        assert!(text.contains(">- line50"));
        assert!(text.contains(">+ changed50"));
        // 3 context each side + the change
        assert!(text.lines().count() <= EXCERPT_MAX_LINES);
        assert!(text.contains("   line47"));
        assert!(text.contains("   line53"));
    }

    #[test]
    fn test_range_selection_payload_and_excerpt() {
        // Rows: ctx(a), modified(b->B), added(C), ctx(z)
        let rows = align("a\nb\nz\n", "a\nB\nC\nz\n");
        let temp = tempfile::tempdir().unwrap();
        let mut s = session(temp.path());

        // Select the whole changed block (rows 1..=2)
        s.stage(FeedbackKind::Comment, "f.rs", &rows, 1, 2, "range");
        let item = &s.pending[0];
        assert_eq!(item.old_line, Some(2));
        assert_eq!(item.new_line, Some(2));
        assert_eq!(item.old_range, Some([2, 2]));
        assert_eq!(item.new_range, Some([2, 3]));

        // Every selected row is marked in the excerpt
        let marked = item
            .hunk_text
            .lines()
            .filter(|line| line.starts_with('>'))
            .count();
        assert!(marked >= 3, "got:\n{}", item.hunk_text);
        assert!(item.hunk_text.contains(">+ C"));
        // Context outside the selection is unmarked
        assert!(item.hunk_text.contains("   a"));
    }

    #[test]
    fn test_wrap_body_spaces_and_cjk() {
        // Breaks at spaces when possible
        let wrapped = wrap_body("alpha beta gamma delta", 12, 12);
        assert_eq!(wrapped, vec!["alpha beta", "gamma delta"]);

        // First line has a smaller budget than continuations
        let wrapped = wrap_body("alpha beta gamma", 6, 12);
        assert_eq!(wrapped, vec!["alpha", "beta gamma"]);

        // CJK: no spaces, hard break on display width (2 cells per char)
        let wrapped = wrap_body("あいうえおかきく", 8, 8);
        assert_eq!(wrapped, vec!["あいうえ", "おかきく"]);

        // Existing newlines are preserved, empty input yields one line
        assert_eq!(wrap_body("a\n\nb", 10, 10), vec!["a", "", "b"]);
        assert_eq!(wrap_body("", 10, 10), vec![""]);
    }

    #[test]
    fn test_note_anchoring() {
        let rows = align("a\nb\n", "a\nB\n");
        let note = Note {
            reply_to: None,
            file: "f".into(),
            old_line: None,
            new_line: Some(2),
            body: "hi".into(),
            author: "agent".into(),
            pending: false,
        };
        assert!(!note.anchors_to(&rows[0]));
        assert!(note.anchors_to(&rows[1]));

        let old_side = Note {
            new_line: None,
            old_line: Some(2),
            ..note.clone()
        };
        assert!(old_side.anchors_to(&rows[1]));
    }

    #[test]
    fn test_poll_skips_preexisting_and_reads_appends() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join(".tssdiff");
        std::fs::create_dir_all(&dir).unwrap();
        let reply = dir.join("replies.jsonl");
        std::fs::write(&reply, "{\"file\":\"stale.rs\",\"body\":\"old\"}\n").unwrap();

        let mut s = session(temp.path());
        s.last_poll = Instant::now() - Duration::from_secs(5);
        assert!(!s.poll_replies(), "pre-existing entries must be skipped");
        assert!(s.notes.is_empty());

        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&reply)
            .unwrap();
        writeln!(
            file,
            "{{\"reply_to\":\"fb-1\",\"file\":\"src/a.rs\",\"new_line\":3,\"body\":\"answer\"}}"
        )
        .unwrap();
        drop(file);

        s.last_poll = Instant::now() - Duration::from_secs(5);
        assert!(s.poll_replies());
        assert_eq!(s.notes.len(), 1);
        assert_eq!(s.notes[0].file, "src/a.rs");
        assert_eq!(s.notes[0].new_line, Some(3));
        assert_eq!(s.notes[0].author, "agent");

        // Nothing new: no change
        s.last_poll = Instant::now() - Duration::from_secs(5);
        assert!(!s.poll_replies());
    }

    #[test]
    fn test_first_send_resets_reply_file() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join(".tssdiff");
        std::fs::create_dir_all(&dir).unwrap();
        let reply = dir.join("replies.jsonl");
        std::fs::write(&reply, "stale\n").unwrap();

        let mut s = session(temp.path());
        s.reset_reply_file_once().unwrap();
        assert_eq!(std::fs::read_to_string(&reply).unwrap(), "");
        assert_eq!(s.read_offset, 0);
        // Self-ignoring directory
        assert_eq!(
            std::fs::read_to_string(dir.join(".gitignore"))
                .unwrap()
                .trim(),
            "*"
        );
    }

    #[test]
    fn test_batch_flush_appends_all_items() {
        let temp = tempfile::tempdir().unwrap();
        let rows = align("a\nb\nc\n", "A\nb\nC\n");
        let mut s = session(temp.path());
        s.stage(FeedbackKind::Comment, "f.rs", &rows, 0, 0, "first");
        s.stage(FeedbackKind::Comment, "g.rs", &rows, 2, 2, "second");
        assert_eq!(s.pending_count(), 2);

        let config = AgentConfig {
            sink: SinkKind::File,
            ..Default::default()
        };
        let status = s.flush(&config).unwrap();
        assert!(status.contains("2 item"), "got: {status}");

        let outbox = temp.path().join(".tssdiff").join("outbox.jsonl");
        let content = std::fs::read_to_string(outbox).unwrap();
        // One JSON line carries the whole batch
        assert_eq!(content.trim().lines().count(), 1);
        let batch: serde_json::Value = serde_json::from_str(content.trim()).unwrap();
        let items = batch["items"].as_array().unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0]["comment"], "first");
        assert_eq!(items[1]["comment"], "second");

        // Both drafts became sent inline notes ("you"), queue emptied
        assert_eq!(s.notes.len(), 2);
        assert!(s.notes.iter().all(|n| !n.pending && n.author == "you"));
        assert!(s.pending.is_empty());
    }

    #[test]
    fn test_flush_empty_is_error() {
        let temp = tempfile::tempdir().unwrap();
        let mut s = session(temp.path());
        let config = AgentConfig {
            sink: SinkKind::File,
            ..Default::default()
        };
        assert!(s.flush(&config).is_err());
    }

    #[test]
    fn test_flush_failure_keeps_drafts() {
        let temp = tempfile::tempdir().unwrap();
        let rows = align("a\n", "b\n");
        let mut s = session(temp.path());
        s.stage(FeedbackKind::Comment, "f.rs", &rows, 0, 0, "note");
        // Command sink with no configured command fails
        let config = AgentConfig {
            sink: SinkKind::Command,
            ..Default::default()
        };
        assert!(s.flush(&config).is_err());
        // Drafts preserved so nothing is lost on retry
        assert_eq!(s.pending_count(), 1);
        assert!(s.notes[0].pending);
    }

    #[test]
    fn test_discard_pending_clears_drafts() {
        let temp = tempfile::tempdir().unwrap();
        let rows = align("a\n", "b\n");
        let mut s = session(temp.path());
        s.stage(FeedbackKind::Comment, "f.rs", &rows, 0, 0, "one");
        s.stage(FeedbackKind::Question, "f.rs", &rows, 0, 0, "two");
        assert_eq!(s.discard_pending(), 2);
        assert!(s.pending.is_empty());
        assert!(s.notes.is_empty());
    }

    #[test]
    fn test_markdown_batch_includes_reply_instruction() {
        let temp = tempfile::tempdir().unwrap();
        let rows = align("a\n", "b\n");
        let mut s = session(temp.path());
        s.stage(FeedbackKind::Question, "f.rs", &rows, 0, 0, "why?");
        s.stage(FeedbackKind::Comment, "f.rs", &rows, 0, 0, "nit");
        let batch = FeedbackBatch {
            version: PAYLOAD_VERSION,
            repo: "repo".into(),
            reply_file: "repo/.tssdiff/replies.jsonl".into(),
            timestamp: 0,
            items: s.pending.clone(),
        };
        let text = format_markdown_batch(&batch);
        assert!(text.contains("2 items"));
        assert!(text.contains("Question"));
        assert!(text.contains("Comment"));
        assert!(text.contains("```diff"));
        assert!(text.contains("replies.jsonl"));
        // Only the question yields a reply line
        assert_eq!(text.matches("reply_to").count(), 1);
    }
}
