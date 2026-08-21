//! The frame renderer: paint the workspace into a fresh `screen::Screen`, diff it against the previous
//! frame, and emit only the changed cells (F-006). Pure over the screen buffer and `Write`.

use std::io::{self, Write};

use crossterm::style::Print;
use crossterm::{cursor, queue, terminal};

use ruse_core::{Command, Mode, SelectKind, SplitDir, Workspace};

use crate::ui::layout::Rect;
use crate::{graphics, highlight, screen};

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

/// Advance a display column past one grapheme cluster `g` — the ONE column rule shared by painting
/// ([`paint_pane`]), the caret ([`cursor_cell`] via [`line_display_col`]), and the inverse
/// ([`line_byte_at_col`]), so the three can never drift. `col` is relative to the line's left edge; a tab
/// expands to the next `TAB_WIDTH` stop, every other cluster advances by its display width. Conceal and
/// virt_text are handled in [`paint_pane`]'s walk (skip / inject), so this stays the pure column rule.
#[inline]
pub(crate) fn advance_col(col: u16, g: &str) -> u16 {
    if g == "\t" {
        col + (TAB_WIDTH - (col % TAB_WIDTH))
    } else {
        col + screen::cluster_width(g)
    }
}

/// The display column (0-based, from the line's left edge) of the grapheme boundary at byte offset
/// `line_byte` within a single line `line` (no trailing `\n`). `line_byte` past the end clamps to the
/// end. The caret's column ([`cursor_cell`]) and the P1/P2 coordinate tests go through here.
pub(crate) fn line_display_col(line: &str, line_byte: usize) -> u16 {
    use unicode_segmentation::UnicodeSegmentation;
    let mut col: u16 = 0;
    for (i, g) in line.grapheme_indices(true) {
        if i >= line_byte {
            break;
        }
        col = advance_col(col, g);
    }
    col
}

/// Inverse of [`line_display_col`]: the byte offset of the grapheme boundary at or just past display
/// column `col` within `line`. A `col` beyond the line maps to its end. The P1 round-trip test exercises
/// it now; a later slice wires it into mouse/click resolution (`byte_of(row, col)`), so it is the seam's
/// other half kept beside its forward map rather than being introduced late.
#[allow(dead_code)] // exercised by the P1 test now; the click/byte_of path lands in a later F-031 slice
pub(crate) fn line_byte_at_col(line: &str, col: u16) -> usize {
    use unicode_segmentation::UnicodeSegmentation;
    let mut cur: u16 = 0;
    for (i, g) in line.grapheme_indices(true) {
        if cur >= col {
            return i;
        }
        cur = advance_col(cur, g);
    }
    line.len()
}

/// Paint one buffer view into its `rect`: `rect.h` lines from `top`, one GRAPHEME CLUSTER per cell at
/// its true display width (F-006 #4), clipped to the rectangle. Tabs expand to the next stop measured
/// from the pane's left edge; a wide glyph that would straddle the right edge is dropped and the rest
/// of that line is skipped. `sel`/`block` paint the Visual selection in reverse video; `byte_style`
/// carries the syntax FACE per byte (fg + bold/italic; empty ⇒ default) — F-031 slice 0. `conceal` byte
/// ranges are HIDDEN from layout (0 cells, following columns shift) EXCEPT on `caret_line`, which is
/// revealed so its markup is visible and editable — F-031 slice 1 reveal-at-point.
#[allow(clippy::too_many_arguments)] // painting one pane legitimately needs the full cell context
pub(crate) fn paint_pane(
    cur: &mut screen::Screen,
    rect: Rect,
    bytes: &[u8],
    byte_style: &[screen::CellStyle],
    top: usize,
    sel: Option<(usize, usize)>,
    block: Option<&[(usize, usize)]>,
    hl: &[(usize, usize)],
    underline: &[(usize, usize)],
    conceal: &[(usize, usize, Option<&str>)],
    caret_line: usize,
    virt: &[highlight::VirtLine],
    out_images: &mut Vec<(String, graphics::Placement)>,
    graphics_on: bool,
    image_dims: &std::collections::HashMap<String, (u32, u32)>,
) {
    use crossterm::style::Color;
    use unicode_segmentation::UnicodeSegmentation;
    // A graphics-capable terminal shows the real image via Unicode PLACEHOLDER cells (§8.8): the cell grid
    // itself carries the image, so tmux renders it at the correct pane position. Otherwise paint the text
    // placeholder box.
    let paint_block = |cur: &mut screen::Screen, disp_row: u16, vb: &highlight::VirtLine| match (
        graphics_on,
        &vb.path,
    ) {
        // A graphics terminal shows the real image ONLY when its pixel dimensions were read (the PNG exists
        // and parsed). If the path is missing/unreadable there are no dims — paint a text "not found" band
        // instead of a blank placeholder grid, so a broken link reads as an error rather than empty space.
        (true, Some(path)) => match image_dims.get(path).copied() {
            Some(dims) => paint_image_cells(cur, rect, disp_row, vb.height, path, Some(dims)),
            None => paint_missing_image(cur, rect, disp_row, vb.height, path),
        },
        _ => paint_virt_block(cur, rect, disp_row, vb),
    };
    if rect.w == 0 || rect.h == 0 {
        return;
    }
    let (x0, x1) = (rect.x, rect.x + rect.w);
    let text = std::str::from_utf8(bytes).unwrap_or("<binary>");
    let mut line: usize = 0;
    let mut scol: u16 = x0;
    // Display rows added by virtual blocks (image placeholders) on lines ABOVE the current one — every
    // subsequent buffer line paints that many rows lower (F-031 slice 3a row-coordinate model).
    let mut virt_before: u16 = 0;
    for (i, g) in text.grapheme_indices(true) {
        if g == "\n" {
            // Finished buffer line `line`: if it is on-screen and carries a virtual block, paint the block
            // on the rows just below its text and grow the running offset.
            if line >= top {
                let disp = (line - top) as u16 + virt_before;
                if let Some(vb) = virt.iter().find(|v| v.after_line == line) {
                    paint_block(cur, disp + 1, vb);
                    collect_image(
                        out_images,
                        rect,
                        disp + 1,
                        vb,
                        vb.path.as_deref().and_then(|p| image_dims.get(p)).copied(),
                    );
                    virt_before += vb.height;
                }
            }
            line += 1;
            scol = x0;
            if line >= top && (line - top) as u16 + virt_before >= rect.h {
                break; // next line is past the bottom of this pane (display rows include virtual ones)
            }
            continue;
        }
        if line < top || scol >= x1 {
            continue; // above the pane, or past its right edge (truncate)
        }
        let disp = (line - top) as u16 + virt_before;
        if disp >= rect.h {
            continue; // clipped below the pane by the virtual rows above
        }
        let srow = rect.y + disp;
        // F-031 conceal: on a non-caret line, a concealed cluster paints NO cell and does not advance the
        // column (following glyphs shift left) — the caret line is revealed so its markup stays editable.
        // At the range's START, any virt_text (a bullet/checkbox glyph) is painted in the marker's place.
        if line != caret_line {
            if let Some(&(s, _e, virt)) = conceal.iter().find(|&&(s, e, _)| i >= s && i < e) {
                if i == s {
                    if let Some(vt) = virt {
                        for vg in vt.graphemes(true) {
                            if scol >= x1 {
                                break;
                            }
                            scol = cur.put(srow, scol, vg, Color::Reset, false);
                        }
                    }
                }
                continue;
            }
        }
        let selected = sel.is_some_and(|(s, e)| i >= s && i < e)
            || block.is_some_and(|rows| rows.iter().any(|&(s, e)| i >= s && i < e))
            || hl.iter().any(|&(s, e)| i >= s && i < e);
        // The per-byte syntax face, plus the frame-local overlays (selection reverse, diagnostic underline).
        let base = byte_style.get(i).copied().unwrap_or_default();
        let underlined = underline.iter().any(|&(s, e)| i >= s && i < e); // F-014 diagnostic range
        if g == "\t" {
            for _ in 0..(advance_col(scol - x0, "\t") - (scol - x0)) {
                if scol >= x1 {
                    break;
                }
                scol = cur.put(srow, scol, " ", base.fg, selected);
            }
        } else if scol + screen::cluster_width(g) > x1 {
            scol = x1; // a wide glyph would cross the edge — drop it and skip the rest of the line
        } else {
            let style = screen::CellStyle {
                reverse: selected,
                underline: base.underline || underlined,
                ..base
            };
            scol = cur.put_styled(srow, scol, g, &style);
        }
    }
    // A virtual block on the FINAL buffer line (a file that ends without a trailing newline) never hits the
    // `\n` branch above, so paint it here.
    if line >= top {
        let disp = (line - top) as u16 + virt_before;
        if disp < rect.h {
            if let Some(vb) = virt.iter().find(|v| v.after_line == line) {
                paint_block(cur, disp + 1, vb);
                collect_image(
                    out_images,
                    rect,
                    disp + 1,
                    vb,
                    vb.path.as_deref().and_then(|p| image_dims.get(p)).copied(),
                );
            }
        }
    }
}

/// The image's `(left_col, cols)` within the pane: sized to the image's natural aspect (from its pixel
/// dimensions) and CENTRED horizontally, rather than stretched to full width. Unknown dimensions fall back
/// to full width. Both painting and `collect_image` go through this so the placeholder cells and the virtual
/// placement agree. F-031 slice 3b-2c.
fn image_layout(rect: Rect, rows: u16, dims: Option<(u32, u32)>) -> (u16, u16) {
    let cols = match dims {
        Some((w, h)) => graphics::fit_cols(w, h, rows, 0.5, rect.w.min(graphics::MAX_PLACEHOLDER)),
        None => rect.w.min(graphics::MAX_PLACEHOLDER),
    };
    let left = rect.x + rect.w.saturating_sub(cols) / 2;
    (left, cols)
}

/// Record an image block's on-screen placement (F-031 slice 3b-2c) for the render loop's graphics pass —
/// only for a block that carries a `path`. Sized to the image's aspect and centred (see [`image_layout`]).
fn collect_image(
    out: &mut Vec<(String, graphics::Placement)>,
    rect: Rect,
    disp_row: u16,
    vb: &highlight::VirtLine,
    dims: Option<(u32, u32)>,
) {
    // No dims ⇒ the image could not be loaded; it renders as a text "not found" band, so record NO
    // graphics placement (a transmit would fail anyway) — keeps the graphics pass in sync with the cells.
    let Some(dims) = dims else { return };
    if let Some(path) = &vb.path {
        let (left, cols) = image_layout(rect, vb.height, Some(dims));
        out.push((
            path.clone(),
            graphics::Placement {
                row: rect.y + disp_row,
                col: left,
                cols,
                rows: vb.height,
            },
        ));
    }
}

/// Paint an image's Unicode PLACEHOLDER cells (F-031 slice 3b-2c): `rows × cols` cells, each holding the
/// placeholder char + row/col diacritics with `fg` = the image id encoded as RGB. The terminal composites
/// the image slice into each cell, so positioning rides the normal cell grid (correct inside a tmux pane).
/// Sized to the image's aspect and centred (see [`image_layout`]).
fn paint_image_cells(
    cur: &mut screen::Screen,
    rect: Rect,
    disp_row: u16,
    rows: u16,
    path: &str,
    dims: Option<(u32, u32)>,
) {
    let id = graphics::image_id(path);
    let (r, g, b) = graphics::id_rgb(id);
    let style = screen::CellStyle {
        fg: crossterm::style::Color::Rgb { r, g, b },
        ..screen::CellStyle::default()
    };
    let (left, cols) = image_layout(rect, rows, dims);
    for ir in 0..rows {
        if disp_row + ir >= rect.h {
            break;
        }
        let srow = rect.y + disp_row + ir;
        for ic in 0..cols {
            cur.put_styled(srow, left + ic, &graphics::placeholder_cell(ir, ic), &style);
        }
    }
}

/// Paint an image PLACEHOLDER block (F-031 slice 3a): `vb.height` rows from display row `disp_row` within
/// `rect` — a `🖼 label` line, then rule lines — clipped to the pane. This is the degrade placeholder rung;
/// a graphics-capable terminal replaces it with real pixels in slice 3b (INV-CAP-DEGRADE).
fn paint_virt_block(cur: &mut screen::Screen, rect: Rect, disp_row: u16, vb: &highlight::VirtLine) {
    use crossterm::style::Color;
    use unicode_segmentation::UnicodeSegmentation;
    let (x0, x1) = (rect.x, rect.x + rect.w);
    for r in 0..vb.height {
        let disp = disp_row + r;
        if disp >= rect.h {
            break;
        }
        let srow = rect.y + disp;
        if r == 0 {
            let label = format!("\u{1f5bc} {}", vb.label); // 🖼 alt
            let mut col = x0;
            for gph in label.graphemes(true) {
                if col >= x1 {
                    break;
                }
                col = cur.put(srow, col, gph, Color::DarkGrey, false);
            }
        } else {
            for col in x0..x1 {
                cur.put(srow, col, "\u{2500}", Color::DarkGrey, false); // ─
            }
        }
    }
}

/// Paint the "image could not be loaded" fallback band (F-031): `[image: <path> (not found)]` in red on
/// the first row, blank rows below, filling `vb.height` rows from `disp_row`. Shown on a graphics terminal
/// when the path is missing/unreadable, so a broken link is legible rather than a silent empty band.
fn paint_missing_image(
    cur: &mut screen::Screen,
    rect: Rect,
    disp_row: u16,
    height: u16,
    path: &str,
) {
    use crossterm::style::Color;
    use unicode_segmentation::UnicodeSegmentation;
    let (x0, x1) = (rect.x, rect.x + rect.w);
    for r in 0..height {
        let disp = disp_row + r;
        if disp >= rect.h {
            break;
        }
        let srow = rect.y + disp;
        if r == 0 {
            let label = format!("[image: {path} (not found)]");
            let mut col = x0;
            for gph in label.graphemes(true) {
                if col >= x1 {
                    break;
                }
                col = cur.put(srow, col, gph, Color::Red, false);
            }
        }
        // rows below the label stay blank (already cleared by the frame)
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
    virt_lines: &[highlight::VirtLine],
    rects: &[Rect],
    prev: &mut screen::Screen,
    sync: bool,
    focus_hl: &[(usize, usize)],
    palette_rows: &[(String, bool)],
    terminals: &TermViews,
    diagnostics: &[crate::lsp::Diag],
    completion: Option<(&[crate::lsp::protocol::CompletionItem], usize)>,
    graphics_on: bool,
    image_dims: &std::collections::HashMap<String, (u32, u32)>,
) -> io::Result<Vec<(String, graphics::Placement)>> {
    use crossterm::style::Color;

    let (cols, rows) = terminal::size().unwrap_or((80, 24));
    let text_rows = rows.saturating_sub(1);
    // Paint the whole frame into a fresh cell grid; the diff against `prev` emits only what changed.
    let mut cur = screen::Screen::new(cols, rows);
    // The focused pane's visible image blocks (path + screen placement), returned for the graphics pass
    // that draws real pixels after the cell flush (F-031 slice 3b-2b).
    let mut images: Vec<(String, graphics::Placement)> = Vec::new();

    // Flatten the focused buffer's highlight spans into a per-byte FACE (spans index into it). Panes
    // showing the SAME buffer reuse it; a pane on a different buffer (post-MVP) renders unstyled. Longest
    // spans come first (highlight orders them), so this last-wins flatten lets the shortest/most-specific
    // capture win — the slice-0 stand-in for the decoration model's priority resolution (F-031).
    let focus = ws.focused();
    let focus_doc = focus.view.doc();
    let fbytes = focus.doc.bytes();
    let mut byte_style = vec![screen::CellStyle::default(); fbytes.len()];
    for s in spans {
        for slot in byte_style
            .iter_mut()
            .take(s.end.min(fbytes.len()))
            .skip(s.start)
        {
            *slot = s.style;
        }
    }
    // F-031 conceal: the byte ranges the providers marked hidden (Markdown heading `# ` prefixes today).
    // Disabled by `RUSE_CONCEAL=off` (the config-file `render.conceal` key has no loader yet — same seam
    // as RUSE_PROFILE). The revealed line is the focused caret's buffer line (reveal-at-point).
    let conceal_on = std::env::var("RUSE_CONCEAL").as_deref() != Ok("off");
    let conceal: Vec<(usize, usize, Option<&'static str>)> = if conceal_on {
        spans
            .iter()
            .filter(|s| s.conceal)
            .map(|s| (s.start, s.end, s.virt))
            .collect()
    } else {
        Vec::new()
    };
    let caret_line = fbytes[..focus.view.cursor().min(fbytes.len())]
        .iter()
        .filter(|&&c| c == b'\n')
        .count();

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
        let style: &[screen::CellStyle] = if p.view.doc() == focus_doc {
            &byte_style
        } else {
            &[]
        };
        let sel = p.view.selection_span(pbytes);
        let block = p.view.block_spans(pbytes);
        // The focused pane also paints the `:s///c` confirm match / incsearch+hlsearch matches (F-009
        // #1) in reverse video, on top of any live Visual selection.
        let hl: &[(usize, usize)] = if i == ws.focus() { focus_hl } else { &[] };
        // Conceal applies to the focused pane's view of the focused doc (F-031); other panes render markup.
        let pane_conceal: &[(usize, usize, Option<&'static str>)] =
            if i == ws.focus() && p.view.doc() == focus_doc {
                &conceal
            } else {
                &[]
            };
        // Virtual blocks (image placeholders) likewise paint into the focused pane's view (F-031 slice 3a).
        let pane_virt: &[highlight::VirtLine] = if i == ws.focus() && p.view.doc() == focus_doc {
            virt_lines
        } else {
            &[]
        };
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
            style,
            p.view.top(),
            sel,
            block.as_deref(),
            hl,
            &underline,
            pane_conceal,
            caret_line,
            pane_virt,
            &mut images,
            graphics_on,
            image_dims,
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

    // F-014 #5: the completion popup menu (pum) — a bordered box anchored at the cursor, drawn over the
    // buffer into the cell grid (the diff emits it; clearing it next frame repaints the covered cells).
    if let Some((items, selected)) = completion {
        if !items.is_empty() {
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
            // Anchor at the focused cursor's screen cell (same math the terminal cursor uses below).
            let (crow, ccol) = cursor_cell(
                focus.doc.bytes(),
                focus.view.cursor(),
                focus.view.top(),
                virt_lines,
            );
            let frect = rects.get(ws.focus()).copied().unwrap_or(Rect {
                x: 0,
                y: 0,
                w: cols,
                h: text_rows,
            });
            let arow = frect.y + crow;
            let acol = frect.x + ccol;
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
            out.flush()?;
            return Ok(images);
        }
        let (row, col) = cursor_cell(
            focus.doc.bytes(),
            focus.view.cursor(),
            focus.view.top(),
            virt_lines,
        );
        let screen_row = (frect.y + row).min(rows.saturating_sub(1));
        let screen_col = (frect.x + col).min(cols.saturating_sub(1));
        queue!(out, cursor::MoveTo(screen_col, screen_row), cursor::Show)?;
    }
    out.flush()?;
    Ok(images)
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
/// count (F-006 #4). `virt` adds the display rows of any virtual blocks (image placeholders) sitting
/// between `top` and the caret's line, so the caret stays aligned with the painted rows (F-031 slice 3a).
pub(crate) fn cursor_cell(
    bytes: &[u8],
    pos: usize,
    top: usize,
    virt: &[highlight::VirtLine],
) -> (u16, u16) {
    let pos = pos.min(bytes.len());
    let buf_row = bytes[..pos].iter().filter(|&&c| c == b'\n').count();
    let line_start = bytes[..pos]
        .iter()
        .rposition(|&c| c == b'\n')
        .map_or(0, |i| i + 1);
    // `line` runs from its start up to `pos`, so the caret column is the full display width of that
    // slice — computed by the SHARED column rule, not a private re-derivation (F-031 slice 0).
    let line = std::str::from_utf8(&bytes[line_start..pos]).unwrap_or("");
    let col = line_display_col(line, line.len());
    // Virtual rows inserted for lines in `[top, buf_row)` push the caret down (F-031 slice 3a).
    let virt_above: u16 = virt
        .iter()
        .filter(|v| v.after_line >= top && v.after_line < buf_row)
        .map(|v| v.height)
        .sum();
    (buf_row.saturating_sub(top) as u16 + virt_above, col)
}

#[cfg(test)]
mod render_tests {
    use super::*;
    use crate::ui::layout::window_rects;
    use crossterm::style::Color;
    use proptest::prelude::*;

    // ---- F-031 slice 0: the display-coordinate layout seam (P1/P2/P6) + faces ----

    proptest! {
        /// P1 (round-trip) + P2 (monotonic): the shared column rule [`line_display_col`] and its inverse
        /// [`line_byte_at_col`] agree, and the column never decreases in buffer order — over arbitrary
        /// lines mixing ASCII, tabs, wide CJK (가), and a combining mark (´).
        #[test]
        fn layout_col_roundtrips_and_is_monotonic(line in "[a-z\t \u{ac00}\u{0301}]{0,40}") {
            use unicode_segmentation::UnicodeSegmentation;
            let boundaries: Vec<usize> = line
                .grapheme_indices(true)
                .map(|(i, _)| i)
                .chain(std::iter::once(line.len()))
                .collect();
            let mut prev = 0u16;
            for &b in &boundaries {
                let c = line_display_col(&line, b);
                prop_assert!(c >= prev, "P2: column decreased at byte {}", b); // monotonic
                prev = c;
                let back = line_byte_at_col(&line, c);
                // P1: col -> byte -> col lands on the same column (robust to zero-width clusters).
                prop_assert_eq!(line_display_col(&line, back), c, "P1: round-trip diverged at byte {}", b);
            }
        }
    }

    /// P6 (identity): the caret column is the documented rule — tabs to the next stop, wide clusters
    /// count 2, a combining mark folds into its base's 1 — pinned by explicit expectations so the slice-0
    /// refactor (paint and caret now share ONE `advance_col` rule) cannot silently drift the layout.
    #[test]
    fn cursor_cell_column_matches_documented_rule() {
        let cases: &[(&str, usize, u16)] = &[
            ("abc", 3, 3),        // plain ASCII
            ("a\tb", 2, 4),       // 'a' -> col 1, tab -> next stop 4
            ("\tx", 1, 4),        // leading tab -> col 4
            ("가나", 3, 2),       // one wide (width-2) cluster before the second
            ("e\u{0301}z", 3, 1), // 'e' + combining acute = one cluster, width 1
        ];
        for &(s, pos, want) in cases {
            let (_row, col) = cursor_cell(s.as_bytes(), pos, 0, &[]);
            assert_eq!(col, want, "caret column for {s:?} at byte {pos}");
        }
    }

    /// The faces channel reaches the cell grid: a byte carrying a bold / italic face paints a bold /
    /// italic cell, and a selection's reverse-video merges ON TOP of the face rather than replacing it
    /// (F-031 slice 0 — the decoration model's first attribute).
    #[test]
    fn paint_pane_carries_bold_and_italic_faces() {
        let bytes = b"ab";
        let styles = [
            screen::CellStyle {
                bold: true,
                ..screen::CellStyle::default()
            },
            screen::CellStyle {
                italic: true,
                ..screen::CellStyle::default()
            },
        ];
        let rect = Rect {
            x: 0,
            y: 0,
            w: 8,
            h: 2,
        };
        let mut cur = screen::Screen::new(8, 2);
        paint_pane(
            &mut cur,
            rect,
            bytes,
            &styles,
            0,
            None,
            None,
            &[],
            &[],
            &[],
            0,
            &[],
            &mut Vec::new(),
            false,
            &std::collections::HashMap::new(),
        );
        assert!(cur.cell(0, 0).style.bold, "byte 0 paints bold");
        assert!(cur.cell(0, 1).style.italic, "byte 1 paints italic");

        let mut sel = screen::Screen::new(8, 2);
        paint_pane(
            &mut sel,
            rect,
            bytes,
            &styles,
            0,
            Some((0, 1)),
            None,
            &[],
            &[],
            &[],
            0,
            &[],
            &mut Vec::new(),
            false,
            &std::collections::HashMap::new(),
        );
        let c = sel.cell(0, 0);
        assert!(
            c.style.bold && c.style.reverse,
            "selection reverse merges with the bold face",
        );
    }

    /// F-031 conceal (slice 1): a concealed range is HIDDEN (0 cells, following columns shift) on a
    /// non-caret line, but REVEALED (painted normally) on the caret's own line so its markup stays
    /// editable — the reveal-at-point rule (D-052). Selection/motion are unaffected (core untouched), so
    /// P3/P4/P5 hold by construction.
    #[test]
    fn paint_pane_conceals_off_caret_line_and_reveals_on_it() {
        let bytes = b"abcd\nabcd"; // two identical lines
        let no_style: &[screen::CellStyle] = &[];
        let rect = Rect {
            x: 0,
            y: 0,
            w: 8,
            h: 2,
        };
        let conceal: [(usize, usize, Option<&str>); 2] = [(0, 2, None), (5, 7, None)]; // hide "ab" both lines

        // Caret on line 1: line 0's "ab" is concealed so 'c' shifts to col 0; line 1 is revealed so 'a'
        // stays at col 0.
        let mut s = screen::Screen::new(8, 2);
        paint_pane(
            &mut s,
            rect,
            bytes,
            no_style,
            0,
            None,
            None,
            &[],
            &[],
            &conceal,
            1,
            &[],
            &mut Vec::new(),
            false,
            &std::collections::HashMap::new(),
        );
        assert_eq!(
            s.cell(0, 0).content,
            screen::Content::Cluster("c".into()),
            "off-caret line: concealed 'ab' shifts 'c' to col 0",
        );
        assert_eq!(
            s.cell(1, 0).content,
            screen::Content::Cluster("a".into()),
            "caret line revealed: 'a' stays at col 0",
        );

        // Caret on line 0: now line 0 is revealed, so 'a' stays at col 0 there too.
        let mut s2 = screen::Screen::new(8, 2);
        paint_pane(
            &mut s2,
            rect,
            bytes,
            no_style,
            0,
            None,
            None,
            &[],
            &[],
            &conceal,
            0,
            &[],
            &mut Vec::new(),
            false,
            &std::collections::HashMap::new(),
        );
        assert_eq!(
            s2.cell(0, 0).content,
            screen::Content::Cluster("a".into()),
            "caret line revealed: 'a' stays at col 0",
        );
    }

    /// F-031 virt_text (slice 2): a concealed range carrying virt_text paints the GLYPH in the marker's
    /// place on a non-caret line (`- x` → `• x`), and shows the real marker on the revealed caret line.
    #[test]
    fn paint_pane_virt_text_replaces_concealed_marker_off_caret_line() {
        let bytes = b"- x\n- y"; // two list items; conceal "- " [0,2)/[4,6), virt "• "
        let no_style: &[screen::CellStyle] = &[];
        let rect = Rect {
            x: 0,
            y: 0,
            w: 8,
            h: 2,
        };
        let conceal: [(usize, usize, Option<&str>); 2] = [(0, 2, Some("• ")), (4, 6, Some("• "))];

        // Caret on line 1: line 0 shows the bullet glyph then the text; line 1 (revealed) shows "- y".
        let mut s = screen::Screen::new(8, 2);
        paint_pane(
            &mut s,
            rect,
            bytes,
            no_style,
            0,
            None,
            None,
            &[],
            &[],
            &conceal,
            1,
            &[],
            &mut Vec::new(),
            false,
            &std::collections::HashMap::new(),
        );
        assert_eq!(
            s.cell(0, 0).content,
            screen::Content::Cluster("•".into()),
            "off-caret line: '- ' concealed, '•' painted in its place",
        );
        assert_eq!(
            s.cell(0, 2).content,
            screen::Content::Cluster("x".into()),
            "text follows the 2-cell bullet glyph",
        );
        assert_eq!(
            s.cell(1, 0).content,
            screen::Content::Cluster("-".into()),
            "caret line revealed: raw '-' marker shows",
        );
    }

    /// F-031 slice 3a (row model): virtual blocks above the caret's line shift the caret DOWN by their
    /// height, so the drawn caret stays on its painted row.
    #[test]
    fn cursor_cell_row_accounts_for_virtual_blocks_above() {
        let bytes = b"a\nb\nc"; // buffer lines 0, 1, 2
        let virt = [highlight::VirtLine {
            after_line: 0,
            height: 2,
            label: "img".into(),
            path: None,
        }];
        // Caret on line 2 ('c' at byte 4): row 2 + 2 virtual rows above = screen row 4.
        assert_eq!(cursor_cell(bytes, 4, 0, &virt).0, 4);
        // Caret on line 0 ('a' at byte 0): nothing above, row 0.
        assert_eq!(cursor_cell(bytes, 0, 0, &virt).0, 0);
    }

    /// F-031 slice 3a (row model): a virtual block pushes the buffer lines below it DOWN, and the block's
    /// first row shows the placeholder label.
    #[test]
    fn paint_pane_places_lines_below_a_virtual_block() {
        let bytes = b"a\nb"; // line 0 "a", line 1 "b"
        let no_style: &[screen::CellStyle] = &[];
        let rect = Rect {
            x: 0,
            y: 0,
            w: 8,
            h: 5,
        };
        let virt = [highlight::VirtLine {
            after_line: 0,
            height: 2,
            label: "x".into(),
            path: None,
        }];
        let mut s = screen::Screen::new(8, 5);
        paint_pane(
            &mut s,
            rect,
            bytes,
            no_style,
            0,
            None,
            None,
            &[],
            &[],
            &[],
            9,
            &virt,
            &mut Vec::new(),
            false,
            &std::collections::HashMap::new(),
        );
        assert_eq!(
            s.cell(0, 0).content,
            screen::Content::Cluster("a".into()),
            "line 0 at row 0",
        );
        assert_eq!(
            s.cell(1, 0).content,
            screen::Content::Cluster("\u{1f5bc}".into()),
            "placeholder label (🖼) on the first block row",
        );
        assert_eq!(
            s.cell(3, 0).content,
            screen::Content::Cluster("b".into()),
            "line 1 pushed below the 2-row block",
        );
    }

    /// F-031: on a graphics terminal, an image whose path could not be loaded (no dims) paints the
    /// `[image: … (not found)]` text band, NOT blank placeholder cells, and records no graphics placement.
    #[test]
    fn paint_pane_shows_text_fallback_for_a_missing_image() {
        let bytes = b"a\nb";
        let no_style: &[screen::CellStyle] = &[];
        let rect = Rect {
            x: 0,
            y: 0,
            w: 40,
            h: 5,
        };
        let virt = [highlight::VirtLine {
            after_line: 0,
            height: 2,
            label: "alt".into(),
            path: Some("/no/such.png".into()),
        }];
        let mut s = screen::Screen::new(40, 5);
        let mut images = Vec::new();
        paint_pane(
            &mut s,
            rect,
            bytes,
            no_style,
            0,
            None,
            None,
            &[],
            &[],
            &[],
            9,
            &virt,
            &mut images,
            true,                              // graphics ON, but the image can't be loaded
            &std::collections::HashMap::new(), // no dims recorded → treated as not-found
        );
        // Row 1 (just below line 0) begins the fallback band with the "[image:" text.
        assert_eq!(
            s.cell(1, 0).content,
            screen::Content::Cluster("[".into()),
            "fallback band starts with '['",
        );
        let row: String = (0..7)
            .map(|c| match &s.cell(1, c).content {
                screen::Content::Cluster(g) => g.clone(),
                _ => " ".into(),
            })
            .collect();
        assert_eq!(row, "[image:", "not-found text band, not placeholder cells");
        assert!(
            images.is_empty(),
            "an unloadable image records no graphics placement"
        );
    }

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
