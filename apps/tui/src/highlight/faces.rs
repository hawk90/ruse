//! Capture name to cell style: the syntax and markup faces.

use crossterm::style::Color;

use crate::screen::CellStyle;

/// The FACE for a tree-sitter capture: its foreground colour (as before) plus text attributes. Slice 0
/// of F-031 adds the attribute channel — comments render italic and keywords bold, the way most editors
/// style them — proving the decoration model carries more than colour. Colours are unchanged from the
/// previous `color_for`, so non-attributed captures paint byte-identically (the P6 identity guard).
pub(crate) fn face_for(name: &str) -> CellStyle {
    // F-031 markup faces (Markdown/Org): a heading is a bold title; slice 1b adds the inline faces the
    // INLINE grammar drives — emphasis (italic), strong (bold), code span (a distinct fg), and links (a
    // blue underline). These are exact-match names (the `.`-head fallback below would flatten them to a
    // bare "markup" head), so they are handled before the split — mirroring `markup.heading`.
    match name {
        "markup.heading" => {
            return CellStyle {
                fg: Color::Yellow,
                bold: true,
                ..CellStyle::default()
            };
        }
        "markup.strong" => {
            return CellStyle {
                bold: true,
                ..CellStyle::default()
            };
        }
        "markup.emphasis" => {
            return CellStyle {
                italic: true,
                ..CellStyle::default()
            };
        }
        "markup.code" => {
            return CellStyle {
                fg: Color::Green,
                ..CellStyle::default()
            };
        }
        "markup.link" => {
            return CellStyle {
                fg: Color::Blue,
                underline: true,
                ..CellStyle::default()
            };
        }
        "markup.quote" => {
            return CellStyle {
                fg: Color::DarkGrey,
                italic: true,
                ..CellStyle::default()
            };
        }
        _ => {}
    }
    let head = name.split('.').next().unwrap_or(name);
    let fg = match head {
        "keyword" => Color::Magenta,
        "string" => Color::Green,
        "comment" => Color::DarkGrey,
        "type" => Color::Yellow,
        "function" | "constructor" => Color::Blue,
        "constant" | "number" => Color::Cyan,
        "attribute" | "label" => Color::DarkYellow,
        _ => Color::Reset,
    };
    CellStyle {
        fg,
        bold: head == "keyword",
        italic: head == "comment",
        ..CellStyle::default()
    }
}
