# Agent feedback

tssdiff can push review feedback from the diff view to an AI coding
agent, and render the agent's replies inline next to the code. The
design is agent-agnostic: tssdiff emits/consumes neutral JSON and knows
nothing about any specific agent harness.

## UX

Feedback is **staged then flushed as one batch**: you write several
comments/questions across lines and files, then send them together in a
single payload. This keeps a multi-file review coherent - the agent sees
the whole set at once instead of reacting to each remark one at a time.

1. Press `c` in the side-by-side or after-only view: a line cursor
   appears.
2. Move it with `j`/`k` (`d`/`u`: 10 lines, `g`/`G`: first/last).
   `v` drops a range anchor: moving then selects multiple lines
   (Esc lifts the anchor).
3. Press `Enter` to open the input box, type your text.
4. `Tab` toggles the kind: **Comment** (a remark) or **Question**
   (expects a reply).
5. `Enter` **stages** the draft: your text appears immediately as a
   dimmed "draft" note beneath the line, and the line cursor stays put
   so you can move on and stage more. Nothing is sent yet.
6. `S` **sends every staged draft as one batch** through the configured
   sink; the drafts turn into sent notes. `X` discards all staged
   drafts. A `N pending` indicator in the status bar tracks the queue.
7. When an agent appends a reply to the reply file, it shows up inline
   within about a second, wrapped to the pane width. Long replies fold
   to a few lines; `n` expands them.

In the GUI the popover's **ドラフトに追加** button (or `Ctrl+Enter`)
stages a draft; a `N 件のドラフト` segment with **送信** / **破棄**
buttons in the status bar flushes or clears the queue.

Notes are session-scoped: they live in memory, and reply entries left
by previous sessions are ignored on startup.

## Configuration

```yaml
# ~/.config/tssdiff/config.yaml
agent:
  sink: clipboard        # clipboard | file | command
  sinkCommand: ""        # command sink: receives payload JSON on stdin
  sinkTimeoutMs: 5000    # command sink timeout
  outboxFile: ""         # file sink target (default: <repo>/.tssdiff/outbox.jsonl)
```

### Sinks

- **clipboard** (default): formats the whole batch as markdown and
  copies it. Paste it into any agent chat. For questions the text
  includes a machine-followable instruction telling the agent how to
  reply.
- **file**: appends the batch as one JSON line to the outbox file.
  For harnesses that watch a file.
- **command**: spawns `sinkCommand`, writes the batch JSON to its
  stdin, and waits up to `sinkTimeoutMs`. Exit 0 means delivered
  (first stdout line is shown in the status bar); non-zero surfaces
  the first stderr line as the error. Adapters bridging to specific
  agent harnesses (e.g. a message-injection endpoint) plug in here.

## Outbound payload (schema v2)

One JSON **batch** document per flush, carrying every staged item:

```json
{
  "version": 2,
  "repo": "F:/path/to/repo",
  "reply_file": "F:/path/to/repo/.tssdiff/replies.jsonl",
  "timestamp": 1783093812,
  "items": [
    {
      "id": "fb-1783093812-1",
      "kind": "question",
      "file": "src/main.rs",
      "old_line": 120,
      "new_line": 135,
      "old_range": [120, 122],
      "new_range": [135, 137],
      "hunk_text": ">-  old line\n>+  new line\n   context",
      "comment": "Why was this changed?"
    }
  ]
}
```

Envelope:

- `version`: `2`. (v1 was a single flat item with `repo`/`reply_file`/
  `timestamp` inline and no `items`; a one-comment flush is now a batch
  with one item. Adapters should accept both by switching on `version`.)
- `repo`: absolute repository root, forward slashes.
- `reply_file`: absolute path agents should append reply lines to.
- `timestamp`: unix epoch seconds at flush time.
- `items`: one or more reviewed items (order = stage order).

Each item:

- `id`: correlation id; replies reference it via `reply_to`.
- `kind`: `"comment"` or `"question"`.
- `file`: repository-relative path, forward slashes.
- `old_line` / `new_line`: 1-based; either may be null (a deleted line
  has no `new_line`, an added line no `old_line`). For multi-line
  selections these are the first selected line on each side.
- `old_range` / `new_range`: inclusive `[start, end]` spans of the
  selection per side; omitted when that side has no selected lines.
  Single-line selections have `start == end`.
- `hunk_text`: unified-style excerpt of the change; selected lines
  are marked with a leading `>`.

The clipboard sink renders the whole batch to one markdown document
(one section per item, plus a single reply instruction listing every
question's id). The file sink appends the batch as one JSON line. The
command sink writes the batch JSON to the adapter's stdin - so a bridge
that formats and forwards it (e.g. to a message-injection endpoint)
delivers the whole review in a **single** message.

## Inbound replies

Agents append **one JSON object per line** to `reply_file`
(`<repo>/.tssdiff/replies.jsonl`):

```json
{"reply_to": "fb-1783093812-1", "file": "src/main.rs", "new_line": 135, "body": "It fixes ...", "author": "claude"}
```

- `file` + `new_line` (or `old_line`) anchor the note to a diff line
  of the currently viewed file.
- `body`: plain text; newlines render as continuation lines.
- `author`: display name (defaults to `agent`).
- `reply_to`: optional; the payload id being answered. Agents may also
  write unsolicited notes with no `reply_to`.

Lifecycle: on startup tssdiff remembers the file's current size and
only reads entries appended afterwards, so stale conversations never
render against a new diff. The file is truncated on the session's
first question. `.tssdiff/` contains a `.gitignore` with `*` so it
never shows up in `git status`.
