//! Diff the frame against the previous one and emit only the changed cells (F-006 #1). Split from `render.rs` (#1386).

use super::*;

/// Emit only the cells that changed between `cur` and `prev` (F-006 #1). Each changed run is one
/// `MoveTo` then its cells, printed with lazy SGR (colour/reverse) changes; a continuation cell is
/// skipped (the wide glyph to its left already advanced over it). When `sync` is set the whole batch
/// is fenced in DEC synchronized-output (`?2026h`/`l`) so the terminal shows the frame atomically (#3).
pub(crate) fn flush_diff(
    out: &mut impl Write,
    cur: &screen::Screen,
    prev: &screen::Screen,
    sync: bool,
) -> io::Result<()> {
    use crossterm::style::{
        Attribute, ResetColor, SetAttribute, SetBackgroundColor, SetForegroundColor,
    };
    use screen::CellStyle;

    let runs = cur.diff(prev);
    if runs.is_empty() {
        return Ok(()); // nothing changed — no full-screen redraw
    }
    queue!(out, cursor::Hide)?;
    if sync {
        out.write_all(b"\x1b[?2026h")?;
    }
    // Track the currently-emitted style; emit an SGR only when a field changes (lazy, like the original).
    let mut style = CellStyle::default();
    queue!(
        out,
        ResetColor,
        SetAttribute(Attribute::Reset) // clears bold/underline/italic/reverse to a known baseline
    )?;
    for (row, start, cells) in runs {
        queue!(out, cursor::MoveTo(start, row))?;
        for cell in &cells {
            let text: &str = match &cell.content {
                screen::Content::Continuation => continue, // covered by the wide glyph on the left
                screen::Content::Blank => " ",
                screen::Content::Cluster(s) => s,
            };
            let s = &cell.style;
            // Attributes: if any boolean toggled OFF, `Attribute::Reset` is the only portable way back, so
            // reset then re-assert the ones still on (also re-emit colours, which Reset clears).
            let attrs_off = (style.bold && !s.bold)
                || (style.underline && !s.underline)
                || (style.italic && !s.italic)
                || (style.reverse && !s.reverse);
            if attrs_off {
                queue!(out, SetAttribute(Attribute::Reset))?;
                style = CellStyle::default();
            }
            if s.bold && !style.bold {
                queue!(out, SetAttribute(Attribute::Bold))?;
            }
            if s.underline && !style.underline {
                queue!(out, SetAttribute(Attribute::Underlined))?;
            }
            if s.italic && !style.italic {
                queue!(out, SetAttribute(Attribute::Italic))?;
            }
            if s.reverse && !style.reverse {
                queue!(out, SetAttribute(Attribute::Reverse))?;
            }
            if s.fg != style.fg {
                queue!(out, SetForegroundColor(s.fg))?;
            }
            if s.bg != style.bg {
                queue!(out, SetBackgroundColor(s.bg))?;
            }
            style = *s;
            queue!(out, Print(text))?;
        }
    }
    queue!(out, SetAttribute(Attribute::Reset), ResetColor)?;
    if sync {
        out.write_all(b"\x1b[?2026l")?;
    }
    Ok(())
}
