# Agent feedback

tssdiff can push review feedback from the diff view to an AI coding
agent, and render the agent's replies inline next to the code. The
design is agent-agnostic: tssdiff emits/consumes neutral JSON and knows
nothing about any specific agent harness.

## UX

1. Press `c` in the side-by-side view: a line cursor appears.
2. Move it with `j`/`k` (`d`/`u`: 10 lines, `g`/`G`: first/last).
3. Press `Enter` to open the input box, type your text.
4. `Tab` toggles the kind: **Comment** (a remark) or **Question**
   (expects a reply).
5. `Enter` sends through the configured sink; your own text immediately
   appears as an inline note beneath the line (sent marker).
6. When an agent appends a reply to the reply file, it shows up inline
   within about a second.

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

- **clipboard** (default): formats the payload as markdown and copies
  it. Paste it into any agent chat. For questions the text includes a
  machine-followable instruction telling the agent how to reply.
- **file**: appends the payload as one JSON line to the outbox file.
  For harnesses that watch a file.
- **command**: spawns `sinkCommand`, writes the payload JSON to its
  stdin, and waits up to `sinkTimeoutMs`. Exit 0 means delivered
  (first stdout line is shown in the status bar); non-zero surfaces
  the first stderr line as the error. Adapters bridging to specific
  agent harnesses (e.g. a message-injection endpoint) plug in here.

## Outbound payload (schema v1)

One JSON document per send:

```json
{
  "version": 1,
  "id": "fb-1783093812-1",
  "kind": "question",
  "repo": "F:/path/to/repo",
  "file": "src/main.rs",
  "old_line": 120,
  "new_line": 135,
  "hunk_text": ">-  old line\n>+  new line\n   context",
  "comment": "Why was this changed?",
  "reply_file": "F:/path/to/repo/.tssdiff/replies.jsonl",
  "timestamp": 1783093812
}
```

- `id`: correlation id; replies reference it via `reply_to`.
- `kind`: `"comment"` or `"question"`.
- `repo`: absolute repository root, forward slashes.
- `file`: repository-relative path, forward slashes.
- `old_line` / `new_line`: 1-based; either may be null (a deleted line
  has no `new_line`, an added line no `old_line`).
- `hunk_text`: unified-style excerpt of the change; the selected line
  is marked with a leading `>`.
- `reply_file`: absolute path agents should append reply lines to.
- `timestamp`: unix epoch seconds.

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
