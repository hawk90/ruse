//! The frame renderer: paint the workspace into a fresh `screen::Screen`, diff it against the previous
//! frame, and emit only the changed cells (F-006). Pure over the screen buffer and `Write`.

use std::io::{self, Write};

use crossterm::style::Print;
use crossterm::{cursor, queue, terminal};

use ruse_core::{Command, Mode, SelectKind, SplitDir, Workspace};

use crate::ui::layout::Rect;
use crate::ui::window_view::{PaneText, WinView};
use crate::{graphics, highlight, screen};

mod caret;
mod chrome;
mod columns;
mod flush;
mod image;
mod window;

pub(crate) use caret::*;
pub(crate) use chrome::*;
#[cfg(test)]
use columns::*;
pub(crate) use flush::*;
use image::*;
pub(crate) use window::*;

/// The schema default tabstop (`editor.tab_width`), in display columns — the fixed width the renderer
/// tests paint at. Live painting reads each window's own `'tabstop'` from its `WinView`.
#[cfg(test)]
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

/// The pattern a search command carries (turns on hlsearch for it), else `None` (F-009 #1).
pub(crate) fn search_pattern(cmd: &Command) -> Option<String> {
    match cmd {
        Command::Search { pattern, .. } => Some(pattern.clone()),
        Command::SearchNext(p) | Command::SearchPrev(p) => Some(p.clone()),
        // `gd`/`gD` resolve to this; like `*`, the landing turns on hlsearch for the whole-word pattern.
        Command::GotoFirstMatch(p) => Some(p.clone()),
        _ => None,
    }
}

/// All matches of `pattern` in `bytes` as byte spans, for the incsearch/hlsearch reverse-video
/// highlight (F-009 #1). Uses the default search options (case-sensitive magic — the search default);
/// an unrepresentable/malformed pattern highlights nothing (never an error path).
#[allow(clippy::too_many_arguments)] // the frame render legitimately needs the full view context
pub(crate) fn render(
    out: &mut io::Stdout,
    ws: &Workspace,
    cmd_line: Option<(char, &str, Option<usize>)>,
    cmd_menu: &CmdMenu<'_>,
    status: &str,
    spans: &[highlight::Span],
    virt_lines: &[highlight::VirtLine],
    rects: &[Rect],
    wins: &[WinView],
    prev: &mut screen::Screen,
    sync: bool,
    focus_hl: &[(usize, usize)],
    palette_rows: &[(String, bool)],
    terminals: &TermViews,
    diagnostics: &[crate::lsp::Diag],
    completion: Option<(&[crate::lsp::protocol::CompletionItem], usize)>,
    graphics_on: bool,
    image_dims: &std::collections::HashMap<String, (u32, u32)>,
    folds: &[crate::folds::Fold],
    line_idx: &crate::line_index::LineIndex,
    pane_idx: &crate::line_index::PaneIndexes,
    showmode: bool,
    conceal_on: bool,
    flags: &str,
    status_serial: u64,
    question: bool,
    pager: &mut crate::ui::message::Pager,
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
    // Size the per-byte face array to the LAST styled byte, not the whole buffer: the highlighter clips
    // `spans` to the viewport, so this is ~O(viewport) instead of an O(buffer) alloc+zero every frame.
    // paint_pane reads any unstyled index (`byte_style.get(i)`) as the default face, so an array that stops
    // at the last span is byte-for-byte identical to a whole-buffer one — just without the below-viewport tail.
    let style_len = spans
        .iter()
        .map(|s| s.end)
        .max()
        .unwrap_or(0)
        .min(fbytes.len());
    let mut byte_style = vec![screen::CellStyle::default(); style_len];
    for s in spans {
        for slot in byte_style
            .iter_mut()
            .take(s.end.min(style_len))
            .skip(s.start)
        {
            *slot = s.style;
        }
    }
    // F-031 conceal: the byte ranges the providers marked hidden (Markdown heading `# ` prefixes today).
    // Off with `render.conceal = false` in the config (`conceal_on`), or `RUSE_CONCEAL=off` for one run.
    // The revealed line is the focused caret's buffer line (reveal-at-point).
    let conceal_on = conceal_on && std::env::var("RUSE_CONCEAL").as_deref() != Ok("off");
    let conceal: Vec<(usize, usize, Option<&'static str>)> = if conceal_on {
        spans
            .iter()
            .filter(|s| s.conceal)
            .map(|s| (s.start, s.end, s.virt))
            .collect()
    } else {
        Vec::new()
    };
    // The focused caret's buffer row — from the revision-cached line index (O(log n)) rather than an
    // O(cursor-offset) newline scan every frame. `line_idx` is refreshed on this same focused buffer.
    let caret_line = line_idx.line_of(focus.view.cursor().0.min(fbytes.len()));
    // #616: the focused buffer's diagnostics as signs, the most severe one on each line.
    let signs: Vec<(usize, char, Color)> = {
        use crate::lsp::model::Severity;
        let mut by_line: std::collections::BTreeMap<usize, Severity> = Default::default();
        let rank = |s: &Severity| match s {
            Severity::Error => 0,
            Severity::Warning => 1,
            Severity::Info => 2,
            Severity::Hint => 3,
        };
        for d in diagnostics {
            let l = line_idx.line_of(d.start.min(fbytes.len()));
            let e = by_line.entry(l).or_insert(d.severity);
            if rank(&d.severity) < rank(e) {
                *e = d.severity;
            }
        }
        by_line
            .into_iter()
            .map(|(l, s)| match s {
                Severity::Error => (l, 'E', Color::Red),
                Severity::Warning => (l, 'W', Color::Yellow),
                Severity::Info => (l, 'I', Color::Blue),
                Severity::Hint => (l, 'H', Color::Cyan),
            })
            .collect()
    };

    // The focused cursor's cell in its window (the completion menu's anchor and the terminal cursor).
    let focus_cursor = || match wins.get(ws.focus()) {
        Some(win) => cursor_screen(
            PaneText {
                bytes: fbytes,
                idx: line_idx,
            },
            win,
            focus.view.top(),
            folds,
            virt_lines,
            focus.view.cursor().0,
            focus.view.mode(),
        ),
        None => (0, 0),
    };

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
        let sel = p.view.selection_span(pbytes).map(|(s, e)| (s.0, e.0));
        let block: Option<Vec<(usize, usize)>> = p
            .view
            .block_spans(pbytes)
            .map(|rows| rows.into_iter().map(|(s, e)| (s.0, e.0)).collect());
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
        // Folds are per-view; slice 1 collapses only the FOCUSED pane's folds (no dedicated F-*; F-007 carve-out).
        let pane_folds: &[crate::folds::Fold] = if i == ws.focus() { folds } else { &[] };
        // F-014: underline the focused buffer's diagnostic ranges.
        let underline: Vec<(usize, usize)> = if i == ws.focus() {
            diagnostics.iter().map(|d| (d.start, d.end)).collect()
        } else {
            Vec::new()
        };
        // A pane on another buffer than the focused one reads that buffer's cached line index (#1182),
        // which the frame's viewport pass refreshed.
        let mut own_idx = crate::line_index::LineIndex::default();
        let idx = if p.view.doc() == focus_doc {
            line_idx
        } else if let Some(ix) = pane_idx.get(p.view.doc(), p.doc.revision()) {
            ix
        } else {
            own_idx.refresh_for(p.view.doc(), p.doc.revision(), pbytes);
            &own_idx
        };
        let fallback;
        let win = match wins.get(i) {
            Some(w) => w,
            None => {
                fallback = WinView::resolve(
                    p.view,
                    &crate::ui::window_view::DisplayGlobals::default(),
                    usize::from(rect.w),
                    usize::from(rect.h),
                    1,
                    false,
                );
                &fallback
            }
        };
        let input = PaneInput {
            text: PaneText { bytes: pbytes, idx },
            win,
            byte_style: style,
            top: p.view.top(),
            sel,
            block: block.as_deref(),
            hl,
            underline: &underline,
            conceal: pane_conceal,
            caret_line,
            cursor_line: idx.line_of(p.view.cursor().0.min(pbytes.len())),
            virt: pane_virt,
            folds: pane_folds,
            signs: if i == ws.focus() { &signs } else { &[] },
            visual: matches!(p.view.mode(), Mode::Visual { .. } | Mode::Select { .. }),
            graphics_on,
            image_dims,
        };
        paint_window(&mut cur, rect, &input, &mut images);
    }
    draw_separators(&mut cur, rects, ws.split_dir(), cols, text_rows, wins);

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
            let (crow, ccol) = focus_cursor();
            let frect = rects.get(ws.focus()).copied().unwrap_or(Rect {
                x: 0,
                y: 0,
                w: cols,
                h: text_rows,
            });
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
    }

    // `c_CTRL-D` lists the matches in columns just above the command line (#613).
    if let Some(items) = cmd_menu.listing {
        let lines = crate::input::complete::list_columns(items, usize::from(cols));
        let shown = lines.len().min(usize::from(text_rows));
        let top = text_rows - shown as u16;
        for (i, line) in lines.iter().take(shown).enumerate() {
            let row = top + i as u16;
            for x in 0..cols {
                cur.put(row, x, " ", Color::Reset, false);
            }
            cur.put_str(row, 0, line, Color::Reset, false);
        }
    }
    if let Some((entries, selected, col)) = &cmd_menu.pum {
        // The entries align with the completed word's text, one cell right of the prompt glyph.
        let text_col = cmd_line.map_or(0, |(_, text, _)| {
            let head: String = text.chars().take(*col).collect();
            crate::input::display_with_caret(&head, usize::MAX).1 + 1
        });
        paint_cmd_pum(
            &mut cur,
            entries,
            *selected,
            u16::try_from(text_col).unwrap_or(u16::MAX),
            cols,
            text_rows,
        );
    }

    // The command line shows control chars as `^X` (a literal `<CR>` typed with `c_CTRL-V` is `^M`) and
    // puts the terminal caret on its own caret, not at the end (F-026 line editor).
    let cmd_shown = cmd_line.map(|(prefix, text, caret)| {
        let (shown, col) = crate::input::display_with_caret(text, caret.unwrap_or(usize::MAX));
        (format!("{prefix}{shown}"), col)
    });
    pager.prompt = crate::ui::message::Prompt::None;
    let bar = match &cmd_shown {
        Some((shown, _)) => shown.clone(),
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
            // nvim's `%m%r` after the name: `f.txt [+][RO]`; `flags` holds the ones after `[+]`.
            let modified = if focus.doc.is_modified() { "[+]" } else { "" };
            let dirty = if modified.is_empty() && flags.is_empty() {
                String::new()
            } else {
                format!(" {modified}{flags}")
            };
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
            // `'noshowmode'` leaves the mode out (#1151).
            let prefix = if showmode {
                format!("{mode}  {name}{dirty}{win}{diag}  ")
            } else {
                format!("{name}{dirty}{win}{diag}  ")
            };
            // A message too long for the rest of the row is shown whole above a hit-enter prompt, or a
            // file message cut at its start (#1222). One taller than the screen is paged from its top
            // with `-- More --` (#1235).
            use crate::ui::message::{fit, fit_question, Shown};
            let shown = if question {
                fit_question(status, usize::from(cols))
            } else {
                fit(
                    crate::ui::message::width(&prefix),
                    status,
                    usize::from(cols),
                )
            };
            match shown {
                Shown::Fits => format!("{prefix}{status}"),
                Shown::Cut(tail) => format!("{prefix}{tail}"),
                Shown::CutRow(tail) => tail,
                Shown::More(lines) => {
                    let page = usize::from(text_rows);
                    let (first, prompt) = pager.layout(status_serial, lines.len(), page);
                    let shown = &lines[first..lines.len().min(first + page)];
                    let top = text_rows - shown.len() as u16;
                    for (i, line) in shown.iter().enumerate() {
                        let row = top + i as u16;
                        for x in 0..cols {
                            cur.put(row, x, " ", Color::Reset, false);
                        }
                        cur.put_str(row, 0, line, Color::Reset, false);
                    }
                    pager.prompt_text(prompt)
                }
            }
        }
    };
    // Paint the status / command line into the last row (put_str truncates at the right edge).
    cur.put_str(rows.saturating_sub(1), 0, &bar, Color::Reset, false);

    // Diff the finished grid against the previous frame and emit ONLY the changed runs (F-006 #1),
    // wrapped in synchronized output when supported so a big repaint lands atomically (#3).
    flush_diff(out, &cur, prev, sync)?;
    *prev = cur;

    if pager.prompt != crate::ui::message::Prompt::None {
        // nvim leaves the cursor after the prompt.
        let col = u16::try_from(crate::ui::message::width(&bar))
            .unwrap_or(u16::MAX)
            .min(cols.saturating_sub(1));
        queue!(
            out,
            cursor::MoveTo(col, rows.saturating_sub(1)),
            cursor::Show
        )?;
    } else if let Some((_, caret_col)) = cmd_shown {
        let ccol = (u16::try_from(caret_col + 1).unwrap_or(u16::MAX)).min(cols.saturating_sub(1));
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
        let (row, col) = focus_cursor();
        let screen_row = (frect.y + row).min(rows.saturating_sub(1));
        let screen_col = frect.x.saturating_add(col).min(cols.saturating_sub(1)); // #899
        queue!(out, cursor::MoveTo(screen_col, screen_row), cursor::Show)?;
    }
    out.flush()?;
    Ok(images)
}

#[cfg(test)]
mod display_tests;

/// A `LineIndex` over `bytes`, for the renderer tests.
#[cfg(test)]
fn li(bytes: &[u8]) -> crate::line_index::LineIndex {
    let mut idx = crate::line_index::LineIndex::default();
    idx.refresh(ruse_core::Revision(0), bytes);
    idx
}

// ---- F-031 slice 0: the display-coordinate layout seam (P1/P2/P6) + faces ----

#[cfg(test)]
mod render_tests {
    use super::*;

    /// F-009 #1: a search command carries its pattern for hlsearch; other commands do not.
    #[test]
    fn search_pattern_extracts_only_from_search_commands() {
        assert_eq!(
            search_pattern(&ruse_core::Command::SearchNext("foo".into())),
            Some("foo".to_string())
        );
        assert_eq!(search_pattern(&ruse_core::Command::MoveLeft), None);
    }
}

#[cfg(test)]
mod caret_tests;
#[cfg(test)]
mod columns_tests;
#[cfg(test)]
mod flush_tests;
#[cfg(test)]
mod layout_tests;
#[cfg(test)]
mod nvim_parity_tests;
#[cfg(test)]
mod window_tests;
