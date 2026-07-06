use crate::side_by_side::{AlignedRow, DisplayRow, RowKind};
use crate::{App, LeftPane, MenuAction};
use ansi_to_tui::IntoText;
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Wrap},
};

pub fn render_menu_bar(f: &mut Frame, area: Rect, app: &mut App) {
    app.regions.menu_items.clear();

    let entries: [(&str, &str, MenuAction); 5] = [
        ("1", "Files", MenuAction::Files),
        ("2", "History", MenuAction::History),
        ("v", "View", MenuAction::ToggleView),
        ("?", "Help", MenuAction::Help),
        ("q", "Quit", MenuAction::Quit),
    ];

    let brand = " tssdiff  ";
    let mut spans = vec![Span::styled(
        brand,
        Style::default()
            .fg(app.theme.colors.title.0)
            .add_modifier(Modifier::BOLD),
    )];
    let mut x = area.x + brand.len() as u16;

    for (key, label, action) in entries {
        let active = matches!(
            (action, app.left_pane),
            (MenuAction::Files, LeftPane::Files) | (MenuAction::History, LeftPane::History)
        );
        let label_style = if active {
            Style::default()
                .fg(app.theme.colors.tree_selected_fg.0)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(app.theme.colors.text_primary.0)
        };

        let text = format!("[{key}] {label}");
        let width = text.len() as u16;
        spans.push(Span::styled(
            format!("[{key}]"),
            Style::default().fg(app.theme.colors.status_modified.0),
        ));
        spans.push(Span::styled(format!(" {label}"), label_style));
        spans.push(Span::raw("  "));

        app.regions.menu_items.push((
            Rect {
                x,
                y: area.y,
                width,
                height: 1,
            },
            action,
        ));
        x += width + 2;
    }

    let bar = Paragraph::new(Line::from(spans))
        .style(Style::default().bg(app.theme.colors.status_bar_bg.0));
    f.render_widget(bar, area);
}

pub fn render_commit_list(f: &mut Frame, area: Rect, app: &mut App) {
    let mut items: Vec<ListItem> = Vec::with_capacity(app.commits.len() + 1);

    let selected_bg = Style::default().bg(app.theme.colors.tree_selected_bg.0);

    // Virtual entry for the current working tree
    let working_tree_line = Line::from(Span::styled(
        format!(
            "{} Working tree",
            crate::icons::bullet(app.config.icon_mode)
        ),
        Style::default().fg(app.theme.colors.status_added.0),
    ));
    items.push(
        ListItem::new(working_tree_line).style(if app.commit_index == 0 {
            selected_bg
        } else {
            Style::default()
        }),
    );

    for (i, commit) in app.commits.iter().enumerate() {
        let is_selected = app.commit_index == i + 1;
        let line = Line::from(vec![
            Span::styled(
                format!("{} ", commit.hash),
                Style::default().fg(app.theme.colors.status_modified.0),
            ),
            Span::styled(
                commit.subject.clone(),
                if is_selected {
                    Style::default().fg(app.theme.colors.tree_selected_fg.0)
                } else {
                    Style::default().fg(app.theme.colors.text_primary.0)
                },
            ),
        ]);
        items.push(ListItem::new(line).style(if is_selected {
            selected_bg
        } else {
            Style::default()
        }));
    }

    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(format!(" History ({} commits)", app.commits.len()))
                .style(Style::default().fg(app.theme.colors.border_focused.0)),
        )
        .style(Style::default().fg(app.theme.colors.text_primary.0));

    f.render_stateful_widget(list, area, &mut app.commit_list_state);
}

pub fn render_commit_graph(f: &mut Frame, area: Rect, app: &mut App) {
    let visible_height = area.height.saturating_sub(2) as usize;
    let selected = app.graph_selected_row();

    // Keep the selected commit roughly centered
    let max_scroll = app.graph_rows.len().saturating_sub(visible_height);
    let scroll = selected
        .map(|row| row.saturating_sub(visible_height / 2))
        .unwrap_or(0)
        .min(max_scroll);
    app.graph_scroll = scroll;

    let end = (scroll + visible_height).min(app.graph_rows.len());
    let lines: Vec<Line> = app.graph_rows[scroll..end]
        .iter()
        .enumerate()
        .map(|(offset, row)| {
            let is_selected = selected == Some(scroll + offset);
            let mut spans = vec![Span::styled(
                row.graph.clone(),
                Style::default().fg(app.theme.colors.tree_line.0),
            )];
            if let Some(ref hash) = row.hash {
                spans.push(Span::styled(
                    format!("{hash} "),
                    Style::default().fg(app.theme.colors.status_modified.0),
                ));
                if !row.refs.is_empty() {
                    spans.push(Span::styled(
                        format!("{} ", row.refs),
                        Style::default().fg(app.theme.colors.border_focused.0),
                    ));
                }
                spans.push(Span::styled(
                    row.subject.clone(),
                    if is_selected {
                        Style::default().fg(app.theme.colors.tree_selected_fg.0)
                    } else {
                        Style::default().fg(app.theme.colors.text_primary.0)
                    },
                ));
            }
            let line = Line::from(spans);
            if is_selected {
                line.style(Style::default().bg(app.theme.colors.tree_selected_bg.0))
            } else {
                line
            }
        })
        .collect();

    let graph = Paragraph::new(Text::from(lines)).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" Graph - [click: select, again: open]")
            .style(Style::default().fg(app.theme.colors.border.0)),
    );
    f.render_widget(graph, area);
}

pub fn render_commit_input(f: &mut Frame, area: Rect, app: &App) {
    let width = 64.min(area.width.saturating_sub(4));
    let popup = Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(3) / 2,
        width,
        height: 3,
    };

    let cursor = match app.config.icon_mode {
        crate::config::IconMode::Ascii => "_",
        _ => "▏",
    };
    f.render_widget(Clear, popup);
    let input = Paragraph::new(Line::from(vec![
        Span::styled(
            format!(" {}", app.commit_message),
            Style::default().fg(app.theme.colors.text_primary.0),
        ),
        Span::styled(
            cursor,
            Style::default().fg(app.theme.colors.border_focused.0),
        ),
    ]))
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title(" Commit message (Enter: commit, Esc: cancel)")
            .style(Style::default().fg(app.theme.colors.border_focused.0)),
    );
    f.render_widget(input, popup);
}

pub fn render_help_overlay(f: &mut Frame, area: Rect, app: &App) {
    let width = 60.min(area.width.saturating_sub(2));
    let height = 32.min(area.height.saturating_sub(2));
    let popup = Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    };

    let section = |title: &str| {
        Line::from(Span::styled(
            title.to_string(),
            Style::default()
                .fg(app.theme.colors.title.0)
                .add_modifier(Modifier::BOLD),
        ))
    };
    let entry = |key: &str, description: &str| {
        Line::from(vec![
            Span::styled(
                format!("  {key:<16}"),
                Style::default().fg(app.theme.colors.status_modified.0),
            ),
            Span::styled(
                description.to_string(),
                Style::default().fg(app.theme.colors.text_primary.0),
            ),
        ])
    };

    let lines = vec![
        section(" Navigation"),
        entry("Tab / Shift+Tab", "next / previous file・commit"),
        entry("g / G", "first / last entry"),
        entry("Enter", "open commit / toggle directory"),
        entry("Esc", "back (Files <-> History), close help"),
        entry("1 / 2", "Files pane / History pane"),
        Line::default(),
        section(" View"),
        entry("j/k, Up/Down", "scroll diff 1 line (History: select)"),
        entry("d/u, PgDn/PgUp", "scroll diff 10 lines"),
        entry("f / b", "scroll diff 20 lines"),
        entry("h/l, H/L", "scroll horizontally 5 / 20 cols"),
        entry("v", "side-by-side <-> unified diff"),
        entry("x", "condensed (hunks) <-> full file"),
        entry("t", "flat list <-> directory tree"),
        entry("Ctrl+L", "force full repaint"),
        Line::default(),
        section(" Files"),
        entry("/", "filter file list"),
        entry("Space", "mark file as reviewed"),
        entry("s / S", "stage / unstage checked files"),
        entry("C", "commit staged changes"),
        entry("o", "open file in editor"),
        entry("r", "reload diffs"),
        Line::default(),
        section(" Mouse"),
        entry("click", "select entry / menu, click again: open"),
        entry("wheel", "scroll list or diff"),
        entry("Alt+wheel", "scroll horizontally (tilt/Shift if forwarded)"),
        Line::default(),
        entry("q", "quit"),
        entry("?", "toggle this help"),
    ];

    f.render_widget(Clear, popup);
    let help = Paragraph::new(Text::from(lines)).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" Help ")
            .style(Style::default().fg(app.theme.colors.border_focused.0)),
    );
    f.render_widget(help, popup);
}

pub fn render_file_list(f: &mut Frame, area: Rect, app: &mut App) {
    let available_width = area.width.saturating_sub(4) as usize; // Account for borders and padding

    // Get current items based on search mode
    let current_items = app.get_current_file_tree_items();

    let items: Vec<ListItem> = current_items
        .iter()
        .enumerate()
        .map(|(i, tree_item)| {
            let is_selected = i == app.selected_index;
            let bg_style = if is_selected {
                Style::default().bg(app.theme.colors.tree_selected_bg.0)
            } else {
                Style::default()
            };

            // Build tree structure with styled spans
            let mut spans = Vec::new();

            // Build tree prefix using diffnav-style logic (ASCII fallback
            // avoids ambiguous-width glyphs on xterm-like terminals)
            let (vertical_line, last_branch, branch) =
                crate::icons::tree_parts(app.config.icon_mode);
            let mut tree_parts = Vec::new();

            // Add vertical lines for ancestor levels
            // For each ancestor level, show a line if that ancestor is NOT
            // the last child (2 characters per level, like diffnav)
            for i in 0..tree_item.depth {
                if i < tree_item.parent_is_last.len() {
                    if tree_item.parent_is_last[i] {
                        tree_parts.push("  "); // Ancestor was last child, no vertical line (2 spaces)
                    } else {
                        tree_parts.push(vertical_line); // Ancestor has siblings below
                    }
                } else {
                    tree_parts.push("  "); // Default to 2 spaces
                }
            }

            // Add connector for current level (with 1 space padding like diffnav)
            if tree_item.depth > 0 {
                if tree_item.is_last_child {
                    tree_parts.push(last_branch); // Final branch connector
                } else {
                    tree_parts.push(branch); // Branch connector
                }
            }

            let tree_prefix = tree_parts.join("");

            // Add tree prefix with tree line color
            if !tree_prefix.is_empty() {
                spans.push(Span::styled(
                    tree_prefix.clone(),
                    Style::default().fg(app.theme.colors.tree_line.0),
                ));
            }

            // Add checkbox for files (not directories)
            if !tree_item.is_directory {
                let is_checked = app.checked_files.contains(&tree_item.full_path);
                let checkbox = crate::icons::checkbox(is_checked, app.config.icon_mode);
                let checkbox_style = if is_selected {
                    Style::default().fg(app.theme.colors.tree_selected_fg.0)
                } else {
                    Style::default().fg(app.theme.colors.text_primary.0)
                };
                spans.push(Span::styled(checkbox, checkbox_style));
            }

            // Get icon based on item type
            let icon = if tree_item.is_directory {
                crate::icons::directory_icon(tree_item.is_expanded, app.config.icon_mode)
            } else {
                // File - use file_diff icon or default
                tree_item
                    .file_diff
                    .as_ref()
                    .map(|fd| fd.get_file_icon(app.config.icon_mode))
                    .unwrap_or(crate::icons::file_icon("", app.config.icon_mode))
            };

            // Apply color to directory icon
            if tree_item.is_directory {
                let icon_style = if is_selected {
                    Style::default().fg(app.theme.colors.tree_selected_fg.0)
                } else {
                    Style::default().fg(app.theme.colors.tree_directory.0)
                };
                spans.push(Span::styled(format!("{icon} "), icon_style));
            } else {
                spans.push(Span::raw(format!("{icon} ")));
            }

            // Add file/directory name with appropriate color
            let name_style = if is_selected {
                Style::default().fg(app.theme.colors.tree_selected_fg.0)
            } else if tree_item.is_directory {
                Style::default().fg(app.theme.colors.tree_directory.0)
            } else {
                // Staged files show green; checked (reviewed) files dim
                let is_staged = app.staged_files.contains(&tree_item.full_path);
                let base_color = if is_staged {
                    app.theme.colors.status_added.0
                } else {
                    app.theme.colors.tree_file.0
                };
                let is_checked = app.checked_files.contains(&tree_item.full_path);
                if is_checked {
                    Style::default()
                        .fg(base_color)
                        .add_modifier(ratatui::style::Modifier::DIM)
                } else {
                    Style::default().fg(base_color)
                }
            };

            // Calculate available space for the name
            let tree_prefix_width = tree_prefix.chars().count();
            let checkbox_width = if !tree_item.is_directory {
                crate::icons::checkbox(false, app.config.icon_mode)
                    .chars()
                    .count()
            } else {
                0
            };
            let icon_width = 2; // Icon + space
            let stats_width = if tree_item.file_diff.is_some() { 10 } else { 0 }; // Rough estimate for stats
            let used_width = tree_prefix_width + checkbox_width + icon_width + stats_width;
            let available_name_width = available_width.saturating_sub(used_width);

            // Truncate name if too long
            let display_name = if tree_item.name.chars().count() > available_name_width
                && available_name_width > 3
            {
                let truncated_width = available_name_width.saturating_sub(3);
                let truncated: String = tree_item.name.chars().take(truncated_width).collect();
                format!("{truncated}...")
            } else {
                tree_item.name.clone()
            };

            spans.push(Span::styled(display_name.clone(), name_style));

            // Add stats for files or collapsed directories
            let stats_to_show =
                if tree_item.is_directory && !tree_item.is_expanded && tree_item.dir_file_count > 0
                {
                    // Show directory statistics when collapsed
                    Some(format!(
                        " {} files +{} -{}",
                        tree_item.dir_file_count,
                        tree_item.dir_added_lines,
                        tree_item.dir_removed_lines
                    ))
                } else {
                    tree_item
                        .file_diff
                        .as_ref()
                        .map(|file_diff| file_diff.diff_stats())
                };

            if let Some(stats) = stats_to_show {
                let current_width = tree_prefix.chars().count() +
                                   checkbox_width + // checkbox width (0 for directories, 2 for files)
                                   2 + // icon width
                                   display_name.chars().count();

                let stats_parts: Vec<&str> = stats.split_whitespace().collect();
                let stats_width = stats.chars().count();

                if current_width + stats_width < available_width {
                    let padding = available_width - current_width - stats_width;
                    spans.push(Span::raw(" ".repeat(padding)));

                    // Parse and color the stats
                    for part in stats_parts {
                        if part.starts_with('+') {
                            spans.push(Span::styled(
                                format!("{part} "),
                                Style::default().fg(app.theme.colors.status_added.0),
                            ));
                        } else if part.starts_with('-') {
                            spans.push(Span::styled(
                                part.to_string(),
                                Style::default().fg(app.theme.colors.status_removed.0),
                            ));
                        } else {
                            spans.push(Span::raw(format!("{part} ")));
                        }
                    }
                }
            }

            ListItem::new(Line::from(spans)).style(bg_style)
        })
        .collect();

    // Create title based on search mode
    let title = if app.search_mode {
        if app.search_query.is_empty() {
            format!(
                " Search Mode - Type to filter ({} items)",
                current_items.len()
            )
        } else {
            format!(
                " Search: '{}' ({} items)",
                app.search_query,
                current_items.len()
            )
        }
    } else {
        format!(" Files & Directories ({} items)", current_items.len())
    };

    let file_list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(title)
                .style(Style::default().fg(app.theme.colors.border.0)),
        )
        .style(Style::default().fg(app.theme.colors.text_primary.0));

    f.render_stateful_widget(file_list, area, &mut app.file_list_state);
}

pub fn render_diff_content(f: &mut Frame, area: Rect, app: &mut App) {
    // Clamp scroll values before rendering
    app.clamp_scroll(area.height, area.width);

    // Check if we need to refresh diff with current width for side-by-side display
    // Use actual diff area width for maximum utilization
    if !matches!(
        app.config.get_diff_command_type(),
        crate::config::DiffCommandType::GitDefault
    ) && should_refresh_diff_width(app, area.width)
    {
        // Pass both terminal width and actual area width for flexible template calculation
        if let Ok((terminal_width, _)) = crossterm::terminal::size() {
            app.refresh_diff_with_area_width(area.width, terminal_width);
        } else {
            app.refresh_diff_with_width(area.width);
        }
    }

    // Convert ANSI sequences to ratatui Text if they exist, otherwise use plain text
    let text_content = if app.contains_ansi_codes(&app.diff_output) {
        // Parse ANSI codes using ansi-to-tui
        match app.diff_output.into_text() {
            Ok(text) => text,
            Err(_) => {
                // Fallback to plain text if ANSI parsing fails
                Text::from(app.diff_output.as_str())
            }
        }
    } else {
        // Plain text without ANSI codes
        Text::from(app.diff_output.as_str())
    };

    let diff_content = Paragraph::new(text_content)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(format!(
                    "Diff Content (using {}) - [h/l: scroll, j/k: files, g/G: jump]",
                    app.config.get_diff_display_name()
                ))
                .style(Style::default().fg(app.theme.colors.border.0)),
        )
        .scroll((app.vertical_scroll, app.horizontal_scroll))
        .wrap(Wrap { trim: false });

    f.render_widget(diff_content, area);
}

/// Check if we should refresh the diff with new width
fn should_refresh_diff_width(_app: &App, current_width: u16) -> bool {
    // Only refresh if width has changed significantly (by more than 5 characters)
    // to avoid constant re-rendering
    static mut LAST_WIDTH: u16 = 0;
    unsafe {
        if LAST_WIDTH == 0 || (current_width as i16 - LAST_WIDTH as i16).abs() > 5 {
            LAST_WIDTH = current_width;
            true
        } else {
            false
        }
    }
}

pub fn render_status_line(f: &mut Frame, area: Rect, app: &App) {
    let current_items = app.get_current_file_tree_items();
    let status_spans = if app.left_pane == LeftPane::History {
        if app.commit_index == 0 {
            vec![Span::styled(
                format!(
                    " {} Working tree changes | Enter: open",
                    crate::icons::bullet(app.config.icon_mode)
                ),
                Style::default().fg(app.theme.colors.status_added.0),
            )]
        } else if let Some(commit) = app.commits.get(app.commit_index - 1) {
            vec![
                Span::styled(
                    format!(" {} ", commit.hash),
                    Style::default().fg(app.theme.colors.status_modified.0),
                ),
                Span::styled(
                    format!("{} ", commit.date),
                    Style::default().fg(app.theme.colors.text_secondary.0),
                ),
                Span::raw(commit.subject.clone()),
            ]
        } else {
            vec![Span::raw(" No commit selected")]
        }
    } else if let Some(tree_item) = current_items.get(app.selected_index) {
        let mut spans = Vec::new();

        if tree_item.is_directory {
            spans.push(Span::raw(" : "));
            spans.push(Span::styled(
                tree_item.full_path.clone(),
                Style::default().fg(app.theme.colors.tree_directory.0),
            ));
            spans.push(Span::raw(" | Directory | "));
        } else if let Some(file_diff) = &tree_item.file_diff {
            spans.push(Span::raw(format!(
                " {}: ",
                file_diff.get_file_icon(app.config.icon_mode)
            )));
            spans.push(Span::styled(
                tree_item.full_path.clone(),
                Style::default().fg(app.theme.colors.tree_file.0),
            ));
            spans.push(Span::raw(" | "));

            // Add colored diff stats
            let stats_string = file_diff.diff_stats();
            let stats_parts: Vec<&str> = stats_string.split_whitespace().collect();
            for (i, part) in stats_parts.iter().enumerate() {
                if part.starts_with('+') {
                    spans.push(Span::styled(
                        part.to_string(),
                        Style::default().fg(app.theme.colors.status_added.0),
                    ));
                } else if part.starts_with('-') {
                    spans.push(Span::styled(
                        part.to_string(),
                        Style::default().fg(app.theme.colors.status_removed.0),
                    ));
                } else {
                    spans.push(Span::raw(part.to_string()));
                }
                if i < stats_parts.len() - 1 {
                    spans.push(Span::raw(" "));
                }
            }
            spans.push(Span::raw(" | "));
            if app.staged_files.contains(&tree_item.full_path) {
                spans.push(Span::styled(
                    "staged | ".to_string(),
                    Style::default().fg(app.theme.colors.status_added.0),
                ));
            }
        } else {
            spans.push(Span::raw(format!(
                " : {} | No diff | ",
                tree_item.full_path
            )));
        }

        spans.push(Span::raw(format!(
            "Scroll: {},{}",
            app.vertical_scroll, app.horizontal_scroll
        )));
        spans
    } else {
        vec![Span::raw(" No item selected")]
    };

    let status = Paragraph::new(Line::from(status_spans))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Status")
                .style(Style::default().fg(app.theme.colors.border_focused.0)),
        )
        .style(Style::default().fg(app.theme.colors.status_bar_fg.0))
        .wrap(Wrap { trim: false });

    f.render_widget(status, area);
}

pub fn render_search_box(f: &mut Frame, area: Rect, app: &App) {
    let (search_text, title) = if app.search_input_mode {
        // Currently typing in search
        let text = if app.search_query.is_empty() {
            "Filter files 󰬛 ".to_string()
        } else {
            format!("󰬛 {}", app.search_query)
        };
        (text, " Search (/: search, Enter: confirm, ESC: exit)")
    } else {
        // Search confirmed, showing filtered results
        let text = if app.search_query.is_empty() {
            "󰬛 All files".to_string()
        } else {
            format!("󰬛 Filtered: '{}'", app.search_query)
        };
        (text, " Search Results (/: new search, ESC: exit)")
    };

    let search_style = if app.search_query.is_empty() && app.search_input_mode {
        Style::default()
            .fg(app.theme.colors.text_primary.0)
            .add_modifier(ratatui::style::Modifier::DIM)
    } else {
        Style::default().fg(app.theme.colors.text_primary.0)
    };

    let border_style = if app.search_input_mode {
        Style::default().fg(app.theme.colors.border_focused.0)
    } else {
        Style::default().fg(app.theme.colors.border.0)
    };

    let search_box = Paragraph::new(search_text)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(title)
                .style(border_style),
        )
        .style(search_style);

    f.render_widget(search_box, area);
}

pub fn render_side_by_side(f: &mut Frame, area: Rect, app: &mut App) {
    let Some(rows) = app.aligned_rows.take() else {
        return;
    };
    let display: Vec<DisplayRow> = if app.display_rows.is_empty() && !rows.is_empty() {
        // Defensive: a full mapping when no display order was prepared
        (0..rows.len()).map(DisplayRow::Row).collect()
    } else {
        std::mem::take(&mut app.display_rows)
    };

    // Clamp scrolling to the displayed content
    let visible_height = area.height.saturating_sub(2) as usize;
    let max_vertical = display.len().saturating_sub(visible_height) as u16;
    app.vertical_scroll = app.vertical_scroll.min(max_vertical);

    let start = app.vertical_scroll as usize;
    let end = (start + visible_height).min(display.len());

    // Line number gutter sized for the largest line number
    let max_line_number = rows
        .iter()
        .flat_map(|row| [row.old.as_ref(), row.new.as_ref()])
        .flatten()
        .map(|(number, _)| *number)
        .max()
        .unwrap_or(1);
    let gutter_width = max_line_number.to_string().len().max(3);

    let gap_marker = match app.config.icon_mode {
        crate::config::IconMode::Ascii => "---",
        _ => "···",
    };
    let gap_line = |hidden: usize| {
        Line::from(Span::styled(
            format!("{gap_marker} {hidden} lines hidden (x: full view) {gap_marker}"),
            Style::default()
                .fg(app.theme.colors.text_dim.0)
                .add_modifier(Modifier::DIM),
        ))
    };

    let mut old_lines: Vec<Line> = Vec::with_capacity(end - start);
    let mut new_lines: Vec<Line> = Vec::with_capacity(end - start);
    for entry in &display[start..end] {
        match entry {
            DisplayRow::Row(index) => {
                let row = &rows[*index];
                old_lines.push(side_line(row, true, gutter_width, app));
                new_lines.push(side_line(row, false, gutter_width, app));
            }
            DisplayRow::Gap { hidden } => {
                old_lines.push(gap_line(*hidden));
                new_lines.push(gap_line(*hidden));
            }
        }
    }

    let panes = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(area);

    let after_title = if app.condensed {
        " After - [v: unified, x: full view]"
    } else {
        " After - [v: unified, x: condensed]"
    };
    let border_style = Style::default().fg(app.theme.colors.border.0);
    let before = Paragraph::new(Text::from(old_lines))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Before")
                .style(border_style),
        )
        .scroll((0, app.horizontal_scroll));
    let after = Paragraph::new(Text::from(new_lines))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(after_title)
                .style(border_style),
        )
        .scroll((0, app.horizontal_scroll));

    f.render_widget(before, panes[0]);
    f.render_widget(after, panes[1]);

    app.aligned_rows = Some(rows);
    app.display_rows = display;
}

/// One display line for the old (before) or new (after) pane
fn side_line(row: &AlignedRow, old_side: bool, gutter_width: usize, app: &App) -> Line<'static> {
    let side = if old_side { &row.old } else { &row.new };

    let Some((number, text)) = side else {
        // Line only exists on the other side: keep the row for alignment
        return Line::from(Span::styled(
            format!(
                "{:>gutter_width$} ",
                crate::icons::filler(app.config.icon_mode)
            ),
            Style::default()
                .fg(app.theme.colors.text_dim.0)
                .add_modifier(Modifier::DIM),
        ));
    };

    let (marker, kind_color, row_bg) = match row.kind {
        RowKind::Context => (' ', app.theme.colors.text_primary.0, None),
        RowKind::Removed => (
            '-',
            app.theme.colors.status_removed.0,
            Some(app.theme.colors.diff_removed_bg.0),
        ),
        RowKind::Added => (
            '+',
            app.theme.colors.status_added.0,
            Some(app.theme.colors.diff_added_bg.0),
        ),
        RowKind::Modified => (
            '~',
            app.theme.colors.status_modified.0,
            Some(app.theme.colors.diff_modified_bg.0),
        ),
    };

    let mut spans = vec![
        Span::styled(
            format!("{number:>gutter_width$} "),
            Style::default()
                .fg(app.theme.colors.text_dim.0)
                .add_modifier(Modifier::DIM),
        ),
        Span::styled(format!("{marker} "), Style::default().fg(kind_color)),
    ];

    // Syntax colors when available; the row kind then shows via the
    // background tint instead of the text color
    let highlighted_segments = app.highlighted.as_ref().and_then(|(old_hl, new_hl)| {
        let table = if old_side { old_hl } else { new_hl };
        table.get(number - 1)
    });
    match highlighted_segments {
        Some(segments) => {
            for (color, segment) in segments {
                spans.push(Span::styled(segment.clone(), Style::default().fg(*color)));
            }
        }
        None => spans.push(Span::styled(text.clone(), Style::default().fg(kind_color))),
    }

    let line = Line::from(spans);
    match row_bg {
        Some(bg) => line.style(Style::default().bg(bg)),
        None => line,
    }
}

pub fn render_warning_bar(f: &mut Frame, area: Rect, app: &App) {
    if let Some(ref warning) = app.warning_message {
        let warning_widget = Paragraph::new(Span::styled(
            format!(" {warning}"),
            Style::default().fg(app.theme.colors.status_bar_fg.0),
        ))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Warning")
                .style(Style::default().fg(app.theme.colors.warning_border.0)),
        )
        .style(Style::default().fg(app.theme.colors.status_bar_fg.0));
        f.render_widget(warning_widget, area);
    }
}
