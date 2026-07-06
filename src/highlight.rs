use ratatui::style::Color;
use std::path::Path;
use std::sync::OnceLock;
use syntect::easy::HighlightLines;
use syntect::highlighting::{Theme, ThemeSet};
use syntect::parsing::{SyntaxReference, SyntaxSet};
use syntect::util::LinesWithEndings;

/// Fallback when the configured theme name is unknown
const DEFAULT_THEME: &str = "base16-ocean.dark";

/// One display line as (foreground color, text) segments
pub type HighlightedLines = Vec<Vec<(Color, String)>>;

static SYNTAX_SET: OnceLock<SyntaxSet> = OnceLock::new();
static THEME_SET: OnceLock<ThemeSet> = OnceLock::new();

fn syntax_set() -> &'static SyntaxSet {
    SYNTAX_SET.get_or_init(SyntaxSet::load_defaults_newlines)
}

fn theme_set() -> &'static ThemeSet {
    THEME_SET.get_or_init(ThemeSet::load_defaults)
}

/// Theme names accepted for the `syntax_theme` config value
#[allow(dead_code)]
pub fn available_themes() -> Vec<String> {
    theme_set().themes.keys().cloned().collect()
}

fn find_syntax(filename: &str, sample: &str) -> Option<&'static SyntaxReference> {
    let set = syntax_set();
    let name = Path::new(filename)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(filename);

    // Extension first, then full filename (Makefile etc.), then shebang
    Path::new(name)
        .extension()
        .and_then(|ext| ext.to_str())
        .and_then(|ext| set.find_syntax_by_extension(ext))
        .or_else(|| set.find_syntax_by_extension(name))
        .or_else(|| set.find_syntax_by_first_line(sample.lines().next().unwrap_or("")))
}

fn resolve_theme(theme_name: &str) -> Option<&'static Theme> {
    let themes = &theme_set().themes;
    themes
        .get(theme_name)
        .or_else(|| themes.get(DEFAULT_THEME))
        .or_else(|| themes.values().next())
}

/// Syntax-highlight both versions of a file. None when the language is
/// unknown, letting the caller fall back to plain row coloring.
/// `max_lines` caps how far highlighting runs (state is per-line from
/// the file start, so a cap directly bounds the cost); lines past the
/// cap simply render unhighlighted
pub fn highlight_pair(
    filename: &str,
    old_text: &str,
    new_text: &str,
    theme_name: &str,
    max_lines: Option<usize>,
) -> Option<(HighlightedLines, HighlightedLines)> {
    let syntax = find_syntax(filename, new_text)?;
    let theme = resolve_theme(theme_name)?;

    Some((
        highlight_text(old_text, syntax, theme, max_lines),
        highlight_text(new_text, syntax, theme, max_lines),
    ))
}

fn highlight_text(
    text: &str,
    syntax: &SyntaxReference,
    theme: &Theme,
    max_lines: Option<usize>,
) -> HighlightedLines {
    let set = syntax_set();
    let mut highlighter = HighlightLines::new(syntax, theme);

    LinesWithEndings::from(text)
        .take(max_lines.unwrap_or(usize::MAX))
        .map(|line| {
            match highlighter.highlight_line(line, set) {
                Ok(segments) => segments
                    .into_iter()
                    .map(|(style, segment)| {
                        let fg = style.foreground;
                        (
                            Color::Rgb(fg.r, fg.g, fg.b),
                            segment
                                .trim_end_matches('\n')
                                .trim_end_matches('\r')
                                .to_string(),
                        )
                    })
                    .collect(),
                // A parse hiccup on one line should not lose its text
                Err(_) => vec![(
                    Color::Reset,
                    line.trim_end_matches('\n')
                        .trim_end_matches('\r')
                        .to_string(),
                )],
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_highlight_rust_source() {
        let old_text = "fn main() {}\n";
        let new_text = "fn main() {\n    println!(\"hi\");\n}\n";

        let (old_hl, new_hl) =
            highlight_pair("src/main.rs", old_text, new_text, DEFAULT_THEME, None).unwrap();

        assert_eq!(old_hl.len(), 1);
        assert_eq!(new_hl.len(), 3);
        // Reassembling the segments reproduces the line text
        let line0: String = new_hl[0].iter().map(|(_, s)| s.as_str()).collect();
        assert_eq!(line0, "fn main() {");
        // Keywords get a color distinct from plain text somewhere
        assert!(new_hl[0].len() > 1);
    }

    #[test]
    fn test_unknown_extension_returns_none() {
        assert!(highlight_pair("data.zzz_unknown", "a\n", "b\n", DEFAULT_THEME, None).is_none());
    }

    #[test]
    fn test_unknown_theme_falls_back() {
        assert!(
            highlight_pair("a.rs", "fn x() {}\n", "fn y() {}\n", "no-such-theme", None).is_some()
        );
    }

    #[test]
    fn test_empty_text_highlights_to_empty() {
        let (old_hl, new_hl) =
            highlight_pair("a.rs", "", "fn a() {}\n", DEFAULT_THEME, None).unwrap();
        assert!(old_hl.is_empty());
        assert_eq!(new_hl.len(), 1);
    }
}
