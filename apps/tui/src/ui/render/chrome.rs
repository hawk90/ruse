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
