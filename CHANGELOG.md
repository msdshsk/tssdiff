# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- Agent feedback: stage multiple comments/questions as drafts and send
  them together as one batch instead of one message per remark. TUI:
  `Enter` stages a draft (cursor stays), `S` sends all, `X` discards;
  drafts show as dimmed `✎` notes with a `N pending` status indicator.
  GUI: the popover's **ドラフトに追加** button stages, a status-bar
  segment with **送信** / **破棄** flushes or clears the queue.
- GUI: the file/history tree pane is now resizable - drag the divider
  between it and the diff pane (double-click resets), width persisted.
  Tree items and commit rows gained hover tooltips showing the full
  path / commit message.

### Changed
- Outbound feedback payload is now a v2 batch envelope
  `{version:2, repo, reply_file, timestamp, items:[...]}`. The
  `tssdiff-kuroko-bridge` adapter accepts both v1 and v2 (v2 renders to
  one markdown, one POST). See docs/agent-feedback.md.

## [0.1.0] - 2024-07-01

### Added
- Initial release of ftdv (File Tree Diff Viewer)
- Interactive file tree navigation with directory folding
- Support for multiple diff tools (delta, bat, ydiff, difftastic)
- Template variable system for flexible diff tool configuration
- Search functionality with real-time filtering
- Persistent checkbox state for reviewed files
- Vim-style keyboard navigation
- Customizable themes and colors
- Direct file/directory comparison support
- Git integration with multiple operation modes
- Shell completion support (bash, zsh, fish, etc.)

### Features
- Native git integration without requiring stdin piping
- Lazygit-style configuration system
- Cross-platform support (Linux, macOS, Windows)
- ANSI color support with automatic detection
- Efficient diff rendering with scrolling support

[Unreleased]: https://github.com/yourusername/ftdv/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/yourusername/ftdv/releases/tag/v0.1.0