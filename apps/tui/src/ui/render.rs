//! The frame renderer: paint the workspace into a fresh `screen::Screen`, diff it against the previous
//! frame, and emit only the changed cells (F-006). Pure over the screen buffer and `Write`.

use std::io::{self, Write};

use crossterm::style::Print;
use crossterm::{cursor, queue, terminal};

use ruse_core::{Command, Mode, SelectKind, SplitDir, Workspace};

use crate::ui::layout::Rect;
use crate::{highlight, screen};

/// One indent level's width in display columns — matches the editor's `editor.tab_width` default.
pub(crate) const TAB_WIDTH: u16 = 4;

/// The terminal grids the renderer may paint (F-011), keyed by their placeholder buffer id. Unix-only; on
/// other targets it is an empty map (there are no terminals) so `render`'s signature stays platform-neutral.
#[cfg(unix)]
pub(crate) type TermViews<'a> =
    std::collections::HashMap<ruse_core::DocumentId, &'a crate::term_grid::Grid>;
#[cfg(not(unix))]
pub(crate) type TermViews<'a> = std::collections::HashMap<ruse_core::DocumentId, &'a ()>;

/// Paint a terminal's VT grid into `rect` (F-011 slice 2): one grid cell per screen cell, with its full
/// style. Continuation cells (right half of a wide glyph) are skipped — the wide glyph already covered them.
#[cfg(unix)]
fn paint_grid(cur: &mut screen::Screen, rect: Rect, grid: &crate::term_grid::Grid) {
    let (grows, gcols) = grid.size();
    for r in 0..grows.min(rect.h) {
        for c in 0..gcols.min(rect.w) {
            if let Some((text, style)) = grid.cell(r, c) {
                let g = if text.is_empty() { " " } else { text };
                cur.put_styled(rect.y + r, rect.x + c, g, style);
            }
        }
    }
}

/// Paint one buffer view into its `rect`: `rect.h` lines from `top`, one GRAPHEME CLUSTER per cell at
/// its true display width (F-006 #4), clipped to the rectangle. Tabs expand to the next stop measured
/// from the pane's left edge; a wide glyph that would straddle the right edge is dropped and the rest
/// of that line is skipped. `sel`/`block` paint the Visual selection in reverse video; `byte_color`
/// carries the syntax colour per byte (empty ⇒ default).
#[allow(clippy::too_many_arguments)] // painting one pane legitimately needs the full cell context
pub(crate) fn paint_pane(
    cur: &mut screen::Screen,
    rect: Rect,
    bytes: &[u8],
    byte_color: &[crossterm::style::Color],
    top: usize,
    sel: Option<(usize, usize)>,
    block: Option<&[(usize, usize)]>,
    hl: &[(usize, usize)],
    underline: &[(usize, usize)],
) {
    use crossterm::style::Color;
    use unicode_segmentation::UnicodeSegmentation;
    if rect.w == 0 || rect.h == 0 {
        return;
    }
    let (x0, x1) = (rect.x, rect.x + rect.w);
    let bottom = top + rect.h as usize;
    let text = std::str::from_utf8(bytes).unwrap_or("<binary>");
    let mut line: usize = 0;
    let mut scol: u16 = x0;
    for (i, g) in text.grapheme_indices(true) {
        if g == "\n" {
            line += 1;
            scol = x0;
            if line >= bottom {
                break; // past the bottom of this pane
            }
            continue;
        }
        if line < top || scol >= x1 {
            continue; // above the pane, or past its right edge (truncate)
        }
        let srow = rect.y + (line - top) as u16;
        let selected = sel.is_some_and(|(s, e)| i >= s && i < e)
            || block.is_some_and(|rows| rows.iter().any(|&(s, e)| i >= s && i < e))
            || hl.iter().any(|&(s, e)| i >= s && i < e);
        let fg = byte_color.get(i).copied().unwrap_or(Color::Reset);
        // F-014: a diagnostic range is underlined (keeping its syntax colour, per the chosen style).
        let underlined = underline.iter().any(|&(s, e)| i >= s && i < e);
        if g == "\t" {
            let stop = TAB_WIDTH - ((scol - x0) % TAB_WIDTH); // stops measured from the pane's left
            for _ in 0..stop {
                if scol >= x1 {
                    break;
                }
                scol = cur.put(srow, scol, " ", fg, selected);
            }
        } else if scol + screen::cluster_width(g) > x1 {
            scol = x1; // a wide glyph would cross the edge — drop it and skip the rest of the line
        } else if underlined {
            let style = screen::CellStyle {
                fg,
                reverse: selected,
                underline: true,
                ..screen::CellStyle::default()
            };
            scol = cur.put_styled(srow, scol, g, &style);
        } else {
            scol = cur.put(srow, scol, g, fg, selected);
        }
    }
}

/// Draw the one-cell dividers between adjacent panes (F-007): a `─` band for a horizontal split, a
/// `│` column for a vertical split. Only interior gaps are drawn (never past the text area).
pub(crate) fn draw_separators(
    cur: &mut screen::Screen,
    rects: &[Rect],
    split: SplitDir,
    cols: u16,
    text_rows: u16,
) {
    use crossterm::style::Color;
    for pair in rects.windows(2) {
        match split {
            SplitDir::Horizontal => {
                let y = pair[0].y + pair[0].h;
                if y < text_rows {
                    for x in 0..cols {
                        cur.put(y, x, "─", Color::Reset, false);
                    }
                }
            }
            SplitDir::Vertical => {
                let x = pair[0].x + pair[0].w;
                if x < cols {
                    for y in 0..text_rows {
                        cur.put(y, x, "│", Color::Reset, false);
                    }
                }
            }
        }
    }
}

/// The pattern a search command carries (turns on hlsearch for it), else `None` (F-009 #1).
pub(crate) fn search_pattern(cmd: &Command) -> Option<String> {
    match cmd {
        Command::Search { pattern, .. } => Some(pattern.clone()),
        Command::SearchNext(p) | Command::SearchPrev(p) => Some(p.clone()),
        _ => None,
    }
}

/// All matches of `pattern` in `bytes` as byte spans, for the incsearch/hlsearch reverse-video
/// highlight (F-009 #1). Uses the default search options (case-sensitive magic — the search default);
/// an unrepresentable/malformed pattern or a non-UTF-8 buffer highlights nothing (never an error path).
#[allow(clippy::too_many_arguments)] // the frame render legitimately needs the full view context
pub(crate) fn render(
    out: &mut io::Stdout,
    ws: &Workspace,
    cmd_line: Option<(char, &str)>,
    status: &str,
    spans: &[highlight::Span],
    rects: &[Rect],
    prev: &mut screen::Screen,
    sync: bool,
    focus_hl: &[(usize, usize)],
    palette_rows: &[(String, bool)],
    terminals: &TermViews,
    diagnostics: &[crate::lsp::Diag],
) -> io::Result<()> {
    use crossterm::style::Color;

    let (cols, rows) = terminal::size().unwrap_or((80, 24));
    let text_rows = rows.saturating_sub(1);
    // Paint the whole frame into a fresh cell grid; the diff against `prev` emits only what changed.
    let mut cur = screen::Screen::new(cols, rows);

    // Flatten the focused buffer's highlight spans into a per-byte colour (spans index into it). Panes
    // showing the SAME buffer reuse it; a pane on a different buffer (post-MVP) renders uncoloured.
    let focus = ws.focused();
    let focus_doc = focus.view.doc();
    let fbytes = focus.doc.bytes();
    let mut byte_color = vec![Color::Reset; fbytes.len()];
    for s in spans {
        for slot in byte_color
            .iter_mut()
            .take(s.end.min(fbytes.len()))
            .skip(s.start)
        {
            *slot = s.color;
        }
    }

    // Paint every window into its sub-rectangle; the focused view owns the terminal cursor below.
    for (i, &rect) in rects.iter().enumerate().take(ws.window_count()) {
        let p = ws.pane(i);
        // F-011 slice 2: a terminal window paints its VT grid (colors, cursor, styles), not the empty
        // placeholder document.
        #[cfg(unix)]
        if let Some(&grid) = terminals.get(&p.view.doc()) {
            paint_grid(&mut cur, rect, grid);
            continue;
        }
        let pbytes = p.doc.bytes();
        let color: &[Color] = if p.view.doc() == focus_doc {
            &byte_color
        } else {
            &[]
        };
        let sel = p.view.selection_span(pbytes);
        let block = p.view.block_spans(pbytes);
        // The focused pane also paints the `:s///c` confirm match / incsearch+hlsearch matches (F-009
        // #1) in reverse video, on top of any live Visual selection.
        let hl: &[(usize, usize)] = if i == ws.focus() { focus_hl } else { &[] };
        // F-014: underline the focused buffer's diagnostic ranges.
        let underline: Vec<(usize, usize)> = if i == ws.focus() {
            diagnostics.iter().map(|d| (d.start, d.end)).collect()
        } else {
            Vec::new()
        };
        paint_pane(
            &mut cur,
            rect,
            pbytes,
            color,
            p.view.top(),
            sel,
            block.as_deref(),
            hl,
            &underline,
        );
    }
    draw_separators(&mut cur, rects, ws.split_dir(), cols, text_rows);

    // The command palette (F-004): paint its match rows just above the status line, newest selection
    // in reverse video. Rows beyond the available height are dropped (the list scrolls with selection
    // in a fuller build; MVP shows the top window).
    if !palette_rows.is_empty() {
        let shown = palette_rows.len().min(text_rows.saturating_sub(1) as usize);
        let top = text_rows.saturating_sub(shown as u16); // first palette row
        for (i, (label, selected)) in palette_rows.iter().take(shown).enumerate() {
            let row = top + i as u16;
            // Clear the row then paint the label (reverse for the selected match).
            for x in 0..cols {
                cur.put(row, x, " ", Color::Reset, *selected);
            }
            cur.put_str(row, 1, label, Color::Reset, *selected);
        }
    }

    let bar = match cmd_line {
        Some((prefix, text)) => format!("{prefix}{text}"),
        None => {
            let mode = match focus.view.mode() {
                Mode::Normal => "NORMAL",
                Mode::Insert => "INSERT",
                Mode::Replace => "REPLACE",
                Mode::VirtualReplace => "V-REPLACE",
                Mode::Visual {
                    kind: SelectKind::Charwise,
                } => "VISUAL",
                Mode::Visual {
                    kind: SelectKind::Linewise,
                } => "V-LINE",
                Mode::Visual {
                    kind: SelectKind::Blockwise,
                } => "V-BLOCK",
                Mode::Select {
                    kind: SelectKind::Charwise,
                } => "SELECT",
                Mode::Select {
                    kind: SelectKind::Linewise,
                } => "S-LINE",
                Mode::Select {
                    kind: SelectKind::Blockwise,
                } => "S-BLOCK",
                Mode::Terminal => "TERMINAL",
                Mode::TerminalNormal => "T-NORMAL",
            };
            // Show the FOCUSED buffer's name (multi-buffer, F-007), not the session path — so switching to
            // a scratch buffer reads `[No Name]`, not the file that `path` still points at.
            let name = ws
                .buffer_name(ws.focused_buffer())
                .map_or_else(|| "[No Name]".to_string(), str::to_string);
            let dirty = if focus.doc.is_modified() { " [+]" } else { "" };
            // Show the window position only once split, so the single-window status line is unchanged.
            let win = if ws.window_count() > 1 {
                format!("  [win {}/{}]", ws.focus() + 1, ws.window_count())
            } else {
                String::new()
            };
            // F-014: a diagnostic summary for the focused buffer, when any (errors/warnings).
            let diag = match crate::lsp::counts(diagnostics) {
                (0, 0) => String::new(),
                (e, w) => format!("  [E:{e} W:{w}]"),
            };
            format!("{mode}  {name}{dirty}{win}{diag}  {status}")
        }
    };
    // Paint the status / command line into the last row (put_str truncates at the right edge).
    cur.put_str(rows.saturating_sub(1), 0, &bar, Color::Reset, false);

    // Diff the finished grid against the previous frame and emit ONLY the changed runs (F-006 #1),
    // wrapped in synchronized output when supported so a big repaint lands atomically (#3).
    flush_diff(out, &cur, prev, sync)?;
    *prev = cur;

    if let Some((_, text)) = cmd_line {
        let ccol = (text.chars().count() as u16 + 1).min(cols.saturating_sub(1));
        queue!(
            out,
            cursor::MoveTo(ccol, rows.saturating_sub(1)),
            cursor::Show
        )?;
    } else {
        // The focused view owns the cursor; its DISPLAY column is grapheme-cluster / width based
        // (F-006 #4), offset into the focused window's rectangle (F-007).
        let frect = rects.get(ws.focus()).copied().unwrap_or(Rect {
            x: 0,
            y: 0,
            w: cols,
            h: text_rows,
        });
        // F-011: a focused terminal owns the cursor at its GRID position (and may hide it, e.g. in vim).
        #[cfg(unix)]
        if let Some(&grid) = terminals.get(&ws.focused_buffer()) {
            if grid.cursor_visible() {
                let (gr, gc) = grid.cursor();
                let sr = (frect.y + gr).min(rows.saturating_sub(1));
                let sc = (frect.x + gc).min(cols.saturating_sub(1));
                queue!(out, cursor::MoveTo(sc, sr), cursor::Show)?;
            } else {
                queue!(out, cursor::Hide)?;
            }
            return out.flush();
        }
        let (row, col) = cursor_cell(focus.doc.bytes(), focus.view.cursor(), focus.view.top());
        let screen_row = (frect.y + row).min(rows.saturating_sub(1));
        let screen_col = (frect.x + col).min(cols.saturating_sub(1));
        queue!(out, cursor::MoveTo(screen_col, screen_row), cursor::Show)?;
    }
    out.flush()
}

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

/// The cursor's on-screen `(row, col)`: `row` relative to the viewport `top`, `col` in DISPLAY cells
/// (wide glyphs count 2, combining marks 0, tabs to the next stop) — grapheme-correct, not a char
/// count (F-006 #4).
pub(crate) fn cursor_cell(bytes: &[u8], pos: usize, top: usize) -> (u16, u16) {
    use unicode_segmentation::UnicodeSegmentation;
    let pos = pos.min(bytes.len());
    let row = bytes[..pos].iter().filter(|&&c| c == b'\n').count();
    let line_start = bytes[..pos]
        .iter()
        .rposition(|&c| c == b'\n')
        .map_or(0, |i| i + 1);
    let line = std::str::from_utf8(&bytes[line_start..pos]).unwrap_or("");
    let mut col: u16 = 0;
    for g in line.graphemes(true) {
        if g == "\t" {
            col += TAB_WIDTH - (col % TAB_WIDTH);
        } else {
            col += screen::cluster_width(g);
        }
    }
    (row.saturating_sub(top) as u16, col)
}

#[cfg(test)]
mod render_tests {
    use super::*;
    use crate::ui::layout::window_rects;
    use crossterm::style::Color;

    /// F-009 #1: a search command carries its pattern for hlsearch; other commands do not.
    #[test]
    fn search_pattern_extracts_only_from_search_commands() {
        assert_eq!(
            search_pattern(&ruse_core::Command::SearchNext("foo".into())),
            Some("foo".to_string())
        );
        assert_eq!(search_pattern(&ruse_core::Command::MoveLeft), None);
    }

    /// F-006 #1: an unchanged frame emits ZERO bytes — no full-screen redraw.
    #[test]
    fn an_unchanged_frame_emits_nothing() {
        let mut a = screen::Screen::new(20, 3);
        a.put_str(0, 0, "hello world", Color::Reset, false);
        let mut b = screen::Screen::new(20, 3);
        b.put_str(0, 0, "hello world", Color::Reset, false);
        let mut buf: Vec<u8> = Vec::new();
        flush_diff(&mut buf, &b, &a, false).unwrap();
        assert!(buf.is_empty(), "identical frames must produce no output");
    }

    /// F-006 #1: a one-cell change emits a small, bounded batch containing just the new glyph — not
    /// the whole screen. (A full redraw of 60 cells would be far larger.)
    #[test]
    fn a_one_cell_change_emits_only_that_cell() {
        let mut a = screen::Screen::new(20, 3);
        a.put_str(0, 0, "hello world", Color::Reset, false);
        let mut b = screen::Screen::new(20, 3);
        b.put_str(0, 0, "hello wOrld", Color::Reset, false); // one char differs
        let mut buf: Vec<u8> = Vec::new();
        flush_diff(&mut buf, &b, &a, false).unwrap();
        let s = String::from_utf8_lossy(&buf);
        assert!(s.contains('O'), "the changed glyph is emitted");
        assert!(!s.contains("hello"), "the unchanged run is NOT re-emitted");
    }

    /// F-007: a single window fills the whole text area (the single-pane path is unchanged).
    #[test]
    fn one_window_fills_the_area() {
        let r = window_rects(80, 24, 1, SplitDir::Horizontal);
        assert_eq!(
            r,
            vec![Rect {
                x: 0,
                y: 0,
                w: 80,
                h: 24
            }]
        );
    }

    /// F-007: `:split` tiles two equal horizontal bands with a one-row divider between them; the rows
    /// partition the area exactly (band + divider + band = text_rows).
    #[test]
    fn horizontal_split_tiles_equal_bands_with_a_divider() {
        let r = window_rects(80, 25, 2, SplitDir::Horizontal);
        assert_eq!(r.len(), 2);
        assert_eq!(
            r[0],
            Rect {
                x: 0,
                y: 0,
                w: 80,
                h: 12
            }
        );
        assert_eq!(
            r[1],
            Rect {
                x: 0,
                y: 13,
                w: 80,
                h: 12
            }
        ); // y = 12 (band) + 1 (divider)
        assert_eq!(r[0].h + 1 + r[1].h, 25, "bands + divider fill the height");
    }

    /// F-007: `:vsplit` tiles two columns side by side with a one-column divider; remainder cells go
    /// to the earliest pane so the width is fully used.
    #[test]
    fn vertical_split_tiles_columns_with_a_divider() {
        let r = window_rects(80, 24, 2, SplitDir::Vertical);
        assert_eq!(r.len(), 2);
        // (80 - 1 divider) / 2 = 39 rem 1 → first pane gets the extra column.
        assert_eq!(
            r[0],
            Rect {
                x: 0,
                y: 0,
                w: 40,
                h: 24
            }
        );
        assert_eq!(
            r[1],
            Rect {
                x: 41,
                y: 0,
                w: 39,
                h: 24
            }
        ); // x = 40 (col) + 1 (divider)
        assert_eq!(r[0].w + 1 + r[1].w, 80, "columns + divider fill the width");
    }

    /// F-006 #3: with sync support the batch is fenced in DEC synchronized output (?2026h/l).
    #[test]
    fn sync_output_fences_the_batch_when_supported() {
        let a = screen::Screen::new(10, 1);
        let mut b = screen::Screen::new(10, 1);
        b.put(0, 0, "Z", Color::Reset, false);
        let mut on: Vec<u8> = Vec::new();
        flush_diff(&mut on, &b, &a, true).unwrap();
        assert!(
            on.windows(8).any(|w| w == b"\x1b[?2026h"),
            "begins synchronized output"
        );
        assert!(
            on.windows(8).any(|w| w == b"\x1b[?2026l"),
            "ends synchronized output"
        );
        let mut off: Vec<u8> = Vec::new();
        flush_diff(&mut off, &b, &a, false).unwrap();
        assert!(
            !off.windows(8).any(|w| w == b"\x1b[?2026h"),
            "no fence when unsupported"
        );
    }
}
