use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;

/// UI-toolkit-agnostic color, mirroring the terminal color model
/// (named ANSI colors, indexed palette, truecolor). Frontends map this
/// to their own color type (ratatui `Color`, CSS, ...).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UiColor {
    Reset,
    Black,
    Red,
    Green,
    Yellow,
    Blue,
    Magenta,
    Cyan,
    Gray,
    DarkGray,
    LightRed,
    LightGreen,
    LightYellow,
    LightBlue,
    LightMagenta,
    LightCyan,
    White,
    Indexed(u8),
    Rgb(u8, u8, u8),
}

// Custom Color type that can be serialized/deserialized from config
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ThemeColor(pub UiColor);

impl Serialize for ThemeColor {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self.0 {
            UiColor::Reset => serializer.serialize_str("reset"),
            UiColor::Black => serializer.serialize_str("black"),
            UiColor::Red => serializer.serialize_str("red"),
            UiColor::Green => serializer.serialize_str("green"),
            UiColor::Yellow => serializer.serialize_str("yellow"),
            UiColor::Blue => serializer.serialize_str("blue"),
            UiColor::Magenta => serializer.serialize_str("magenta"),
            UiColor::Cyan => serializer.serialize_str("cyan"),
            UiColor::Gray => serializer.serialize_str("gray"),
            UiColor::DarkGray => serializer.serialize_str("dark_gray"),
            UiColor::LightRed => serializer.serialize_str("light_red"),
            UiColor::LightGreen => serializer.serialize_str("light_green"),
            UiColor::LightYellow => serializer.serialize_str("light_yellow"),
            UiColor::LightBlue => serializer.serialize_str("light_blue"),
            UiColor::LightMagenta => serializer.serialize_str("light_magenta"),
            UiColor::LightCyan => serializer.serialize_str("light_cyan"),
            UiColor::White => serializer.serialize_str("white"),
            UiColor::Indexed(n) => serializer.serialize_str(&format!("color{n}")),
            UiColor::Rgb(r, g, b) => serializer.serialize_str(&format!("#{r:02x}{g:02x}{b:02x}")),
        }
    }
}

struct ThemeColorVisitor;

impl Visitor<'_> for ThemeColorVisitor {
    type Value = ThemeColor;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("a color name or hex code")
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        let color = match value.to_lowercase().as_str() {
            "reset" => UiColor::Reset,
            "black" => UiColor::Black,
            "red" => UiColor::Red,
            "green" => UiColor::Green,
            "yellow" => UiColor::Yellow,
            "blue" => UiColor::Blue,
            "magenta" => UiColor::Magenta,
            "cyan" => UiColor::Cyan,
            "gray" | "grey" => UiColor::Gray,
            "dark_gray" | "dark_grey" => UiColor::DarkGray,
            "light_red" => UiColor::LightRed,
            "light_green" => UiColor::LightGreen,
            "light_yellow" => UiColor::LightYellow,
            "light_blue" => UiColor::LightBlue,
            "light_magenta" => UiColor::LightMagenta,
            "light_cyan" => UiColor::LightCyan,
            "white" => UiColor::White,
            s if s.starts_with("color") => {
                let n = s[5..].parse::<u8>().map_err(de::Error::custom)?;
                UiColor::Indexed(n)
            }
            s if s.starts_with('#') && s.len() == 7 => {
                let r = u8::from_str_radix(&s[1..3], 16).map_err(de::Error::custom)?;
                let g = u8::from_str_radix(&s[3..5], 16).map_err(de::Error::custom)?;
                let b = u8::from_str_radix(&s[5..7], 16).map_err(de::Error::custom)?;
                UiColor::Rgb(r, g, b)
            }
            _ => return Err(de::Error::custom(format!("unknown color: {value}"))),
        };
        Ok(ThemeColor(color))
    }
}

impl<'de> Deserialize<'de> for ThemeColor {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_str(ThemeColorVisitor)
    }
}

impl From<ThemeColor> for UiColor {
    fn from(tc: ThemeColor) -> Self {
        tc.0
    }
}

impl Default for ThemeColor {
    fn default() -> Self {
        ThemeColor(UiColor::White)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ColorScheme {
    // File tree colors
    pub tree_line: ThemeColor,
    pub tree_selected_bg: ThemeColor,
    pub tree_selected_fg: ThemeColor,
    pub tree_directory: ThemeColor,
    pub tree_file: ThemeColor,

    // File status colors
    pub status_added: ThemeColor,
    pub status_removed: ThemeColor,
    pub status_modified: ThemeColor,

    // UI chrome colors
    pub border: ThemeColor,
    pub border_focused: ThemeColor,
    pub title: ThemeColor,
    pub status_bar_bg: ThemeColor,
    pub status_bar_fg: ThemeColor,

    // Text colors
    pub text_primary: ThemeColor,
    pub text_secondary: ThemeColor,
    pub text_dim: ThemeColor,

    // Background colors
    pub background: ThemeColor,

    // Warning colors
    #[serde(default = "default_warning_border")]
    pub warning_border: ThemeColor,

    // Row background tints for the side-by-side panes
    #[serde(default = "default_diff_added_bg")]
    pub diff_added_bg: ThemeColor,
    #[serde(default = "default_diff_removed_bg")]
    pub diff_removed_bg: ThemeColor,
    #[serde(default = "default_diff_modified_bg")]
    pub diff_modified_bg: ThemeColor,
}

fn default_warning_border() -> ThemeColor {
    ThemeColor(UiColor::Yellow)
}

fn default_diff_added_bg() -> ThemeColor {
    ThemeColor(UiColor::Rgb(18, 48, 24))
}

fn default_diff_removed_bg() -> ThemeColor {
    ThemeColor(UiColor::Rgb(58, 22, 22))
}

fn default_diff_modified_bg() -> ThemeColor {
    ThemeColor(UiColor::Rgb(50, 42, 14))
}

impl Default for ColorScheme {
    fn default() -> Self {
        Self::dark_theme()
    }
}

impl ColorScheme {
    /// Default dark theme
    pub fn dark_theme() -> Self {
        Self {
            // File tree colors
            tree_line: ThemeColor(UiColor::DarkGray),
            tree_selected_bg: ThemeColor(UiColor::Rgb(50, 50, 70)),
            tree_selected_fg: ThemeColor(UiColor::Yellow),
            tree_directory: ThemeColor(UiColor::Blue),
            tree_file: ThemeColor(UiColor::White),

            // File status colors
            status_added: ThemeColor(UiColor::Green),
            status_removed: ThemeColor(UiColor::Red),
            status_modified: ThemeColor(UiColor::Yellow),

            // UI chrome colors
            border: ThemeColor(UiColor::DarkGray),
            border_focused: ThemeColor(UiColor::Cyan),
            title: ThemeColor(UiColor::Cyan),
            status_bar_bg: ThemeColor(UiColor::DarkGray),
            status_bar_fg: ThemeColor(UiColor::White),

            // Text colors
            text_primary: ThemeColor(UiColor::White),
            text_secondary: ThemeColor(UiColor::Gray),
            text_dim: ThemeColor(UiColor::DarkGray),

            // Background colors
            background: ThemeColor(UiColor::Black),

            // Warning colors
            warning_border: ThemeColor(UiColor::Yellow),

            // Side-by-side row background tints
            diff_added_bg: default_diff_added_bg(),
            diff_removed_bg: default_diff_removed_bg(),
            diff_modified_bg: default_diff_modified_bg(),
        }
    }
}

/// Theme configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Theme {
    pub name: String,
    pub colors: ColorScheme,
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            name: "dark".to_string(),
            colors: ColorScheme::dark_theme(),
        }
    }
}
