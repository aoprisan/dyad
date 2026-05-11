//! Solarized Dark color palette and the semantic roles dyad's chrome
//! and syntax highlighter map onto it.
//!
//! Palette is Ethan Schoonover's Solarized
//! (<https://ethanschoonover.com/solarized/>). The role functions paint
//! a coherent picture so the renderer doesn't need to know which hex
//! goes where — it just asks for `theme::editor()` or
//! `theme::diagnostic(severity)`.

use ratatui::style::{Color, Modifier, Style};

// ---------- raw palette ----------

pub const BASE03: Color = Color::Rgb(0x00, 0x2b, 0x36); // background
pub const BASE02: Color = Color::Rgb(0x07, 0x36, 0x42); // background highlights / status bar
pub const BASE01: Color = Color::Rgb(0x58, 0x6e, 0x75); // comments / secondary
pub const BASE0: Color = Color::Rgb(0x83, 0x94, 0x96); // primary foreground (dark variant)
pub const BASE1: Color = Color::Rgb(0x93, 0xa1, 0xa1); // emphasized content
pub const YELLOW: Color = Color::Rgb(0xb5, 0x89, 0x00);
pub const ORANGE: Color = Color::Rgb(0xcb, 0x4b, 0x16);
pub const RED: Color = Color::Rgb(0xdc, 0x32, 0x2f);
pub const MAGENTA: Color = Color::Rgb(0xd3, 0x36, 0x82);
pub const VIOLET: Color = Color::Rgb(0x6c, 0x71, 0xc4);
pub const BLUE: Color = Color::Rgb(0x26, 0x8b, 0xd2);
pub const CYAN: Color = Color::Rgb(0x2a, 0xa1, 0x98);
pub const GREEN: Color = Color::Rgb(0x85, 0x99, 0x00);

// ---------- semantic chrome roles ----------

/// Default editor surface — paragraph backdrop for the text area.
pub fn editor() -> Style {
    Style::default().bg(BASE03).fg(BASE0)
}

/// Gutter (line numbers + git markers). Same bg as the editor so the
/// boundary is invisible; fg is dimmer so numbers don't compete with
/// content.
pub fn gutter() -> Style {
    Style::default().bg(BASE03).fg(BASE01)
}

/// Solid status-bar base — used for the path / cursor / dirty segment.
pub fn status_bar() -> Style {
    Style::default()
        .bg(BASE02)
        .fg(BASE1)
        .add_modifier(Modifier::BOLD)
}

/// Quieter variant for keymap hints on the status bar's right side.
pub fn status_hint() -> Style {
    Style::default().bg(BASE02).fg(BASE01)
}

// ---------- file-tree roles ----------

/// Default sidebar surface. Same backdrop as the editor so the split
/// reads as one document with chrome on the side, not a separate panel.
pub fn tree() -> Style {
    Style::default().bg(BASE03).fg(BASE0)
}

/// Selected row in the file tree (when the sidebar is focused).
pub fn tree_selected() -> Style {
    Style::default()
        .bg(BASE02)
        .fg(BASE1)
        .add_modifier(Modifier::BOLD)
}

/// Directory entries — picked out in blue so they read distinctly from
/// files at the same indent level.
pub fn tree_dir() -> Style {
    Style::default().bg(BASE03).fg(BLUE)
}

// ---------- badge / diagnostic / git roles ----------

pub fn ok() -> Color {
    GREEN
}
pub fn warn() -> Color {
    YELLOW
}
pub fn error() -> Color {
    RED
}

/// Map an LSP diagnostic severity (1=error..4=hint) to a foreground
/// color. Falls back to the muted base for unknown values.
pub fn diagnostic(severity: Option<u8>) -> Color {
    match severity {
        Some(1) => RED,
        Some(2) => YELLOW,
        Some(3) => CYAN,
        Some(4) => VIOLET,
        _ => BASE1,
    }
}

// ---------- syntax-highlight roles ----------

/// Color a tree-sitter capture name. Returns the fully-styled `Style`
/// so callers don't need to layer modifiers themselves.
pub fn syntax(primary_capture: &str) -> Style {
    let base = Style::default();
    match primary_capture {
        "comment" => base.fg(BASE01).add_modifier(Modifier::ITALIC),
        "keyword" => base.fg(GREEN).add_modifier(Modifier::BOLD),
        "string" => base.fg(CYAN),
        "number" | "constant" => base.fg(MAGENTA),
        "function" => base.fg(BLUE),
        "type" => base.fg(YELLOW),
        "attribute" => base.fg(ORANGE),
        "property" => base.fg(BLUE),
        "tag" | "label" => base.fg(RED),
        "punctuation" => base.fg(BASE01),
        _ => base,
    }
}
