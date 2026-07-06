use crate::config::IconMode;
use std::path::Path;

/// Icon for a file, in the configured icon set
pub fn file_icon(filename: &str, mode: IconMode) -> char {
    match mode {
        IconMode::Nerd => nerd_file_icon(filename),
        IconMode::Emoji => emoji_file_icon(filename),
        IconMode::Ascii => '·',
    }
}

/// Icon for a directory, in the configured icon set
pub fn directory_icon(expanded: bool, mode: IconMode) -> char {
    match (mode, expanded) {
        (IconMode::Nerd, true) => '\u{f115}',  //  Open folder
        (IconMode::Nerd, false) => '\u{f114}', //  Closed folder
        (IconMode::Emoji, true) => '📂',
        (IconMode::Emoji, false) => '📁',
        (IconMode::Ascii, true) => '-',
        (IconMode::Ascii, false) => '+',
    }
}

fn extension_of(filename: &str) -> String {
    Path::new(filename)
        .extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.to_lowercase())
        .unwrap_or_default()
}

/// Emoji icons render through the OS emoji font fallback, so they work
/// without a Nerd Font installed
fn emoji_file_icon(filename: &str) -> char {
    match filename {
        "Cargo.toml" | "Cargo.lock" => '📦',
        ".gitignore" | ".gitmodules" | ".gitattributes" => '🔧',
        "Makefile" | "makefile" | "CMakeLists.txt" => '🔧',
        "README" | "README.md" => '📖',
        "LICENSE" => '📜',
        "CHANGELOG" | "CHANGELOG.md" => '📝',
        _ => match extension_of(filename).as_str() {
            "rs" => '🦀',
            "py" | "pyc" | "pyo" | "pyw" => '🐍',
            "js" | "jsx" | "mjs" | "cjs" | "ts" | "tsx" => '📜',
            "go" => '🐹',
            "java" | "class" | "jar" => '☕',
            "c" | "h" | "cpp" | "cxx" | "cc" | "hpp" | "hxx" => '🔩',
            "rb" => '💎',
            "sh" | "bash" | "zsh" | "ps1" | "bat" | "cmd" => '🐚',
            "json" | "yaml" | "yml" | "toml" | "ini" | "conf" | "cfg" => '🔧',
            "md" | "markdown" => '📝',
            "txt" | "text" => '📄',
            "html" | "htm" => '🌐',
            "css" | "scss" | "sass" => '🎨',
            "png" | "jpg" | "jpeg" | "gif" | "svg" | "webp" | "ico" | "bmp" => '🖼',
            "zip" | "tar" | "gz" | "7z" | "rar" => '📦',
            "pdf" => '📕',
            "sql" | "db" | "sqlite" => '💾',
            "lock" => '🔒',
            _ => '📄',
        },
    }
}

/// Nerd Font glyphs (private use area); requires a patched terminal font
fn nerd_file_icon(filename: &str) -> char {
    // Check special filenames first
    match filename {
        // Rust
        "Cargo.toml" | "Cargo.lock" => '\u{e7a8}', //

        // Git
        ".gitignore" | ".gitmodules" | ".gitattributes" => '\u{f1d3}', //

        // Build files
        "Makefile" | "makefile" => '\u{e779}', //
        "CMakeLists.txt" => '\u{e779}',        //

        // Config
        ".editorconfig" => '\u{e615}', //

        // Documentation
        "README" | "README.md" => '\u{f48a}',  //
        "LICENSE" | "CHANGELOG" => '\u{f15c}', //
        "CHANGELOG.md" => '\u{f48a}',          //

        _ => {
            match extension_of(filename).as_str() {
                // Programming languages
                "rs" => '\u{e7a8}',                                 //
                "py" | "pyc" | "pyo" | "pyw" => '\u{e73c}',         //
                "js" | "jsx" | "mjs" => '\u{e74e}',                 //
                "ts" | "tsx" => '\u{e628}',                         //
                "go" => '\u{e724}',                                 //
                "java" | "class" | "jar" => '\u{e738}',             //
                "c" | "h" => '\u{e61e}',                            //
                "cpp" | "cxx" | "cc" | "hpp" | "hxx" => '\u{e61d}', //
                "rb" => '\u{e739}',                                 //

                // Config
                "json" => '\u{e60b}',                 //
                "yaml" | "yml" => '\u{f481}',         //
                "toml" => '\u{e615}',                 //
                "ini" | "conf" | "cfg" => '\u{e615}', //

                // Documentation
                "md" | "markdown" => '\u{f48a}', //
                "txt" | "text" => '\u{f15c}',    //

                // Web
                "html" | "htm" => '\u{e736}',          //
                "css" | "scss" | "sass" => '\u{e749}', //

                // Default
                _ => '\u{f15b}', //
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_emoji_icons() {
        assert_eq!(file_icon("main.rs", IconMode::Emoji), '🦀');
        assert_eq!(file_icon("unknown.xyz", IconMode::Emoji), '📄');
        assert_eq!(directory_icon(true, IconMode::Emoji), '📂');
    }

    #[test]
    fn test_ascii_icons() {
        assert_eq!(file_icon("main.rs", IconMode::Ascii), '·');
        assert_eq!(directory_icon(false, IconMode::Ascii), '+');
    }

    #[test]
    fn test_nerd_icons_unchanged() {
        assert_eq!(file_icon("main.rs", IconMode::Nerd), '\u{e7a8}');
    }
}
