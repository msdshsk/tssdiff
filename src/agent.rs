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

/// Schema version of the outbound payload
const PAYLOAD_VERSION: u32 = 1;
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

/// Neutral outbound payload; sinks receive this as one JSON document.
/// The schema is the public contract for command-sink adapters.
#[derive(Debug, Serialize)]
pub struct FeedbackPayload {
    pub version: u32,
    pub id: String,
    pub kind: String,
    /// Absolute repository root (or invocation directory outside a repo)
    pub repo: String,
    /// Repository-relative path of the file under review
    pub file: String,
    pub old_line: Option<usize>,
    pub new_line: Option<usize>,
    /// Inclusive [start, end] line spans when a multi-line range was
    /// selected (schema v1 additive; single lines have start == end)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old_range: Option<[usize; 2]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new_range: Option<[usize; 2]>,
    /// Unified-style excerpt of the change around the selected lines
    pub hunk_text: String,
    pub comment: String,
    /// Absolute path agents should append reply JSON lines to
    pub reply_file: String,
    /// Unix epoch seconds at send time
    pub timestamp: u64,
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
        }
    }

    fn next_feedback_id(&mut self) -> String {
        let id = format!("fb-{}-{}", unix_now(), self.next_id);
        self.next_id += 1;
        id
    }

    /// Build the payload for the selected row range (inclusive row
    /// indices into `rows`; single-line selections have start == end)
    pub fn build_payload(
        &mut self,
        kind: FeedbackKind,
        file: &str,
        rows: &[AlignedRow],
        selection_start: usize,
        selection_end: usize,
        comment: &str,
    ) -> FeedbackPayload {
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
        let span = |lines: &[usize]| -> Option<[usize; 2]> {
            Some([*lines.first()?, *lines.last()?])
        };

        FeedbackPayload {
            version: PAYLOAD_VERSION,
            id: self.next_feedback_id(),
            kind: kind.as_str().to_string(),
            repo: path_string(&self.repo),
            file: file.replace('\\', "/"),
            old_line: old_lines.first().copied(),
            new_line: new_lines.first().copied(),
            old_range: span(&old_lines),
            new_range: span(&new_lines),
            hunk_text: excerpt(rows, selection_start, selection_end),
            comment: comment.to_string(),
            reply_file: path_string(&self.reply_path),
            timestamp: unix_now(),
        }
    }

    /// Send through the configured sink. On success the user's own
    /// feedback is stored as a note (the inline "sent" marker) and a
    /// short status string is returned for the status bar.
    pub fn send(&mut self, config: &AgentConfig, payload: &FeedbackPayload) -> Result<String> {
        let status = match config.sink {
            SinkKind::Clipboard => {
                send_clipboard(&format_markdown(payload))?;
                "copied to clipboard".to_string()
            }
            SinkKind::File => {
                let path = self.prepare_outbox(config)?;
                append_json_line(&path, payload)?;
                format!("appended to {}", path.display())
            }
            SinkKind::Command => {
                self.prepare_session_dir()?;
                send_command(config, payload)?
            }
        };

        // Questions expect replies: reset the transport file so this
        // session only ever sees its own conversation
        if payload.kind == "question" {
            self.reset_reply_file_once()?;
        }

        self.notes.push(Note {
            reply_to: Some(payload.id.clone()),
            file: payload.file.clone(),
            old_line: payload.old_line,
            new_line: payload.new_line,
            body: payload.comment.clone(),
            author: "you".to_string(),
        });
        Ok(status)
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

fn append_json_line(path: &Path, payload: &FeedbackPayload) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create {}", parent.display()))?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("Failed to open {}", path.display()))?;
    let json = serde_json::to_string(payload)?;
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
fn send_command(config: &AgentConfig, payload: &FeedbackPayload) -> Result<String> {
    use std::process::{Command, Stdio};

    let command_str = config.sink_command.trim();
    if command_str.is_empty() {
        return Err(anyhow!(
            "agent.sinkCommand is not configured (set it in config.yaml)"
        ));
    }
    let mut parts = command_str.split_whitespace();
    let program = parts.next().unwrap();

    let mut child = Command::new(program)
        .args(parts)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| anyhow!("Failed to spawn {program}: {e}"))?;

    if let Some(mut stdin) = child.stdin.take() {
        let json = serde_json::to_string(payload)?;
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
            output.lines().next().unwrap_or("no output").trim().to_string()
        } else {
            detail
        };
        Err(anyhow!("{program} failed ({}): {detail}", status))
    }
}

/// Markdown rendering used when tssdiff itself is the bridge
/// (clipboard sink): human-pasteable, with a machine-followable reply
/// instruction for questions.
pub fn format_markdown(payload: &FeedbackPayload) -> String {
    let mut location = payload.file.clone();
    if let Some(new_line) = payload.new_line {
        location.push_str(&format!(":{new_line}"));
    } else if let Some(old_line) = payload.old_line {
        location.push_str(&format!(":{old_line} (old)"));
    }

    let heading = match payload.kind.as_str() {
        "question" => "Question about a diff",
        _ => "Review comment on a diff",
    };

    let mut text = format!(
        "## {heading}: {location}\n\nRepository: `{}`\n\n```diff\n{}\n```\n\n{}\n",
        payload.repo,
        payload.hunk_text.trim_end(),
        payload.comment,
    );

    if payload.kind == "question" {
        let reply = serde_json::json!({
            "reply_to": payload.id,
            "file": payload.file,
            "new_line": payload.new_line,
            "old_line": payload.old_line,
            "body": "<your answer>",
            "author": "<your name>",
        });
        text.push_str(&format!(
            "\n---\nPlease answer by appending ONE line of JSON to `{}` \
             (create the file if needed, keep it one line, do not edit other lines):\n{}\n",
            payload.reply_file, reply
        ));
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
    fn test_payload_schema_fields() {
        let rows = align("a\nb\nc\n", "a\nB\nc\n");
        let temp = tempfile::tempdir().unwrap();
        let mut s = session(temp.path());
        let payload = s.build_payload(FeedbackKind::Question, "src\\lib.rs", &rows, 1, 1, "why?");

        assert_eq!(payload.version, 1);
        assert_eq!(payload.kind, "question");
        assert_eq!(payload.file, "src/lib.rs");
        assert_eq!(payload.old_line, Some(2));
        assert_eq!(payload.new_line, Some(2));
        assert_eq!(payload.old_range, Some([2, 2]));
        assert_eq!(payload.new_range, Some([2, 2]));
        assert!(payload.hunk_text.contains("- b"));
        assert!(payload.hunk_text.contains("+ B"));
        assert!(payload.reply_file.ends_with(".tssdiff/replies.jsonl"));

        let json = serde_json::to_string(&payload).unwrap();
        for key in [
            "version", "id", "kind", "repo", "file", "old_line", "new_line", "hunk_text",
            "comment", "reply_file", "timestamp",
        ] {
            assert!(json.contains(&format!("\"{key}\"")), "missing {key}");
        }
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
        let payload = s.build_payload(FeedbackKind::Comment, "f.rs", &rows, 1, 2, "range");
        assert_eq!(payload.old_line, Some(2));
        assert_eq!(payload.new_line, Some(2));
        assert_eq!(payload.old_range, Some([2, 2]));
        assert_eq!(payload.new_range, Some([2, 3]));

        // Every selected row is marked in the excerpt
        let marked = payload
            .hunk_text
            .lines()
            .filter(|line| line.starts_with('>'))
            .count();
        assert!(marked >= 3, "got:\n{}", payload.hunk_text);
        assert!(payload.hunk_text.contains(">+ C"));
        // Context outside the selection is unmarked
        assert!(payload.hunk_text.contains("   a"));
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

        let mut file = std::fs::OpenOptions::new().append(true).open(&reply).unwrap();
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
            std::fs::read_to_string(dir.join(".gitignore")).unwrap().trim(),
            "*"
        );
    }

    #[test]
    fn test_file_sink_appends_payload() {
        let temp = tempfile::tempdir().unwrap();
        let rows = align("a\n", "b\n");
        let mut s = session(temp.path());
        let payload = s.build_payload(FeedbackKind::Comment, "f.rs", &rows, 0, 0, "note");

        let config = AgentConfig {
            sink: SinkKind::File,
            ..Default::default()
        };
        let status = s.send(&config, &payload).unwrap();
        assert!(status.contains("outbox.jsonl"));

        let outbox = temp.path().join(".tssdiff").join("outbox.jsonl");
        let content = std::fs::read_to_string(outbox).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(content.trim()).unwrap();
        assert_eq!(parsed["kind"], "comment");
        assert_eq!(parsed["comment"], "note");

        // Own feedback becomes an inline note ("sent" marker)
        assert_eq!(s.notes.len(), 1);
        assert_eq!(s.notes[0].author, "you");
    }

    #[test]
    fn test_markdown_question_includes_reply_instruction() {
        let temp = tempfile::tempdir().unwrap();
        let rows = align("a\n", "b\n");
        let mut s = session(temp.path());
        let payload = s.build_payload(FeedbackKind::Question, "f.rs", &rows, 0, 0, "why?");
        let text = format_markdown(&payload);
        assert!(text.contains("## Question"));
        assert!(text.contains("```diff"));
        assert!(text.contains("replies.jsonl"));
        assert!(text.contains("reply_to"));

        let comment = s.build_payload(FeedbackKind::Comment, "f.rs", &rows, 0, 0, "nit");
        let text = format_markdown(&comment);
        assert!(text.contains("## Review comment"));
        assert!(!text.contains("replies.jsonl"));
    }
}
