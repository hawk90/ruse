//! The window separators and the command-line popup menu. Split from `render.rs` (#1386).

use super::*;

/// Draw the one-cell dividers between adjacent panes (F-007): a `─` band for a horizontal split, a
/// `│` column for a vertical split. Only interior gaps are drawn (never past the text area).
pub(crate) fn draw_separators(
    cur: &mut screen::Screen,
    rects: &[Rect],
    split: SplitDir,
    cols: u16,
    text_rows: u16,
    wins: &[WinView],
) {
    use crossterm::style::Color;
    for (i, pair) in rects.windows(2).enumerate() {
        // The window before the divider owns it: its 'fillchars' `vert` / `horiz` (#616).
        let fill = wins.get(i).map(|w| &w.fill);
        let mut buf = [0u8; 4];
        match split {
            SplitDir::Horizontal => {
                let ch: &str = fill.map_or('\u{2500}', |f| f.horiz).encode_utf8(&mut buf);
                let y = pair[0].y + pair[0].h;
                if y < text_rows {
                    for x in 0..cols {
                        cur.put(y, x, ch, Color::Reset, false);
                    }
                }
            }
            SplitDir::Vertical => {
                let ch: &str = fill.map_or('\u{2502}', |f| f.vert).encode_utf8(&mut buf);
                let x = pair[0].x + pair[0].w;
                if x < cols {
                    for y in 0..text_rows {
                        cur.put(y, x, ch, Color::Reset, false);
                    }
                }
            }
        }
    }
}

/// What the open command line shows above itself (#613): the wildmenu's popup menu and the matches
/// `c_CTRL-D` listed.
#[derive(Default)]
pub(crate) struct CmdMenu<'a> {
    /// The popup menu: its entries, the selected one, and the char column of the line (after the prompt
    /// glyph) its text aligns with.
    pub pum: Option<(Vec<&'a str>, Option<usize>, usize)>,
    /// The `c_CTRL-D` listing.
    pub listing: Option<&'a [crate::input::complete::Item]>,
}

/// nvim's `'pumwidth'` default: the popup menu's narrowest text column.
const PUM_MIN_WIDTH: usize = 15;

/// Paint the command line's wildmenu as nvim v0.12.6 does with `wildoptions=pum` (#613): a menu just
/// above the command line, as tall as the screen allows, its text column aligned with the completed
/// word's text (one blank cell before it), at least [`PUM_MIN_WIDTH`] wide, the selected entry in
/// reverse video, and a scrollbar on its right when the entries do not fit.
pub(super) fn paint_cmd_pum(
    cur: &mut screen::Screen,
    entries: &[&str],
    selected: Option<usize>,
    text_col: u16,
    cols: u16,
    text_rows: u16,
) {
    use crossterm::style::Color;
    use unicode_width::UnicodeWidthStr;
    let height = entries.len().min(usize::from(text_rows));
    if height == 0 || cols < 2 {
        return;
    }
    let scroll = entries.len() > height;
    let w = entries
        .iter()
        .map(|e| e.width() + 1)
        .max()
        .unwrap_or(0)
        .max(PUM_MIN_WIDTH);
    let box_w = (w + 1 + usize::from(scroll)).min(usize::from(cols));
    let left = usize::from(text_col.saturating_sub(1)).min(usize::from(cols) - box_w);
    // Keep the selected entry in view: the window ends at it once it passes the bottom.
    let top_item = selected.map_or(0, |s| (s + 1).saturating_sub(height));
    let first_row = text_rows - height as u16;
    let menu = screen::CellStyle {
        bg: Color::DarkGrey,
        ..screen::CellStyle::default()
    };
    let thumb = (
        top_item * height / entries.len(),
        (height * height).div_ceil(entries.len()),
    );
    for r in 0..height {
        let i = top_item + r;
        let style = screen::CellStyle {
            reverse: Some(i) == selected,
            ..menu
        };
        let row = first_row + r as u16;
        let mut text = String::from(" ");
        text.push_str(entries[i]);
        let pad = (w + 1).saturating_sub(text.width());
        text.push_str(&" ".repeat(pad));
        let mut col = left as u16;
        for g in unicode_segmentation::UnicodeSegmentation::graphemes(text.as_str(), true) {
            if usize::from(col) >= left + box_w - usize::from(scroll) {
                break;
            }
            col = cur.put_styled(row, col, g, &style).max(col + 1);
        }
        if scroll {
            let on = r >= thumb.0 && r < thumb.0 + thumb.1;
            let bar = screen::CellStyle {
                bg: if on { Color::Grey } else { Color::DarkGrey },
                ..screen::CellStyle::default()
            };
            cur.put_styled(row, (left + box_w - 1) as u16, " ", &bar);
        }
    }
}

/// The command palette (F-004): paint its match rows just above the status line, newest selection
/// in reverse video. Rows beyond the available height are dropped (the list scrolls with selection
/// in a fuller build; MVP shows the top window).
pub(super) fn paint_palette(
    cur: &mut screen::Screen,
    palette_rows: &[(String, bool)],
    cols: u16,
    text_rows: u16,
) {
    let shown = palette_rows.len().min(text_rows.saturating_sub(1) as usize);
    let top = text_rows.saturating_sub(shown as u16); // first palette row
    for (i, (label, selected)) in palette_rows.iter().take(shown).enumerate() {
        let row = top + i as u16;
        // Clear the row then paint the label (reverse for the selected match).
        paint_row(cur, row, cols, 1, label, *selected);
    }
}

/// F-014 #5: the completion popup menu (pum) — a bordered box anchored at the cursor, drawn over the
/// buffer into the cell grid (the diff emits it; clearing it next frame repaints the covered cells).
/// `(crow, ccol)` is the focused cursor's cell in its window `frect`, where the menu anchors.
pub(super) fn paint_completion(
    cur: &mut screen::Screen,
    items: &[crate::lsp::protocol::CompletionItem],
    selected: usize,
    (crow, ccol): (u16, u16),
    frect: Rect,
    cols: u16,
    text_rows: u16,
) {
    use crossterm::style::Color;
    // One display string per item: "label  detail" (detail is the dimmed type/signature).
    let rows: Vec<String> = items
        .iter()
        .map(|it| match &it.detail {
            Some(d) => format!("{}  {}", it.label, d),
            None => it.label.clone(),
        })
        .collect();
    let iw = rows
        .iter()
        .map(|r| r.chars().count())
        .max()
        .unwrap_or(10)
        .clamp(10, 40);
    let height = rows.len().min(10) as u16;
    let arow = frect.y + crow;
    let acol = frect.x.saturating_add(ccol); // ccol saturates on a >u16 line (#899)
    let box_w = iw as u16 + 2; // + side borders
    let left = acol.min(cols.saturating_sub(box_w));
    // Prefer below the cursor; flip above if the box (content + 2 borders) would overflow the text.
    let below_top = arow + 1;
    let (top, first) = if below_top + height + 2 <= text_rows {
        (below_top, below_top + 1)
    } else {
        let t = arow.saturating_sub(height + 2);
        (t, t + 1)
    };
    let border = |n: usize| "─".repeat(n);
    cur.put_str(top, left, &format!("┌{}┐", border(iw)), Color::Reset, false);
    for (i, text) in rows.iter().take(height as usize).enumerate() {
        let r = first + i as u16;
        let mut content: String = text.chars().take(iw).collect();
        let pad = iw.saturating_sub(content.chars().count());
        content.push_str(&" ".repeat(pad));
        cur.put(r, left, "│", Color::Reset, false);
        cur.put_str(r, left + 1, &content, Color::Reset, i == selected);
        cur.put(r, left + 1 + iw as u16, "│", Color::Reset, false);
    }
    let bottom = first + height;
    cur.put_str(
        bottom,
        left,
        &format!("└{}┘", border(iw)),
        Color::Reset,
        false,
    );
}

/// The command line's own menus: the `c_CTRL-D` listing in columns above it (#613) and the wildmenu
/// popup aligned with the completed word.
pub(super) fn paint_cmd_menus(
    cur: &mut screen::Screen,
    cmd_menu: &CmdMenu<'_>,
    cmd_line: Option<(char, &str, Option<usize>)>,
    cols: u16,
    text_rows: u16,
) {
    // `c_CTRL-D` lists the matches in columns just above the command line (#613).
    if let Some(items) = cmd_menu.listing {
        let lines = crate::input::complete::list_columns(items, usize::from(cols));
        let shown = lines.len().min(usize::from(text_rows));
        let top = text_rows - shown as u16;
        for (i, line) in lines.iter().take(shown).enumerate() {
            let row = top + i as u16;
            paint_row(cur, row, cols, 0, line, false);
        }
    }
    if let Some((entries, selected, col)) = &cmd_menu.pum {
        // The entries align with the completed word's text, one cell right of the prompt glyph.
        let text_col = cmd_line.map_or(0, |(_, text, _)| {
            let head: String = text.chars().take(*col).collect();
            crate::input::display_with_caret(&head, usize::MAX).1 + 1
        });
        paint_cmd_pum(
            cur,
            entries,
            *selected,
            u16::try_from(text_col).unwrap_or(u16::MAX),
            cols,
            text_rows,
        );
    }
}

/// Clear screen row `row` across all `cols` and write `text` from column `x`, reversed when `reverse`
/// (a palette selection). The rows painted over the windows above the status line use it.
pub(super) fn paint_row(
    cur: &mut screen::Screen,
    row: u16,
    cols: u16,
    x: u16,
    text: &str,
    reverse: bool,
) {
    use crossterm::style::Color;
    for c in 0..cols {
        cur.put(row, c, " ", Color::Reset, reverse);
    }
    cur.put_str(row, x, text, Color::Reset, reverse);
}
