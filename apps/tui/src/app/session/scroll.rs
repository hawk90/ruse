//! The window-row keys (#615): the scroll commands (`CTRL-E`/`CTRL-Y`, `CTRL-F`/`CTRL-B` and the page
//! keys, `CTRL-D`/`CTRL-U`) and the screen-line motions (`gj`, `gk`, `g0`, `g^`, `gm`, `g$`), both run
//! over the focused window's screen rows. Split from `session.rs` (#1496).

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ruse_core::{Command, Workspace};

use super::{Editing, FrameCtx};
use crate::app::dispatch::{is_ctrl, place_cursor};
use crate::input::{Feed, InputEngine};
use crate::line_index;
use crate::ui::window_view::{PaneLines, PaneText, WinView};

/// The rows of context a window keeps (`'scrolloff'`, #1151): its own value when it has one that is not
/// negative (nvim's `get_scrolloff_value`), else the global one; a negative global keeps none.
pub(super) fn scrolloff_for(view: &ruse_core::View, global: i64) -> usize {
    let so = view.scrolloff_local().filter(|&n| n >= 0).unwrap_or(global);
    usize::try_from(so).unwrap_or(0)
}

/// The page-scroll direction a Normal/Visual key names (`true` = forward), nvim v0.12.6 `:help CTRL-F`:
/// `C-f`/`<PageDown>`/`<S-Down>`/`<S-CR>` forward, `C-b`/`<PageUp>`/`<S-Up>` back. `C-f`/`C-b` are left to
/// the Native profile's pickers when `native`. `<S-+>`/`<S-NL>` are not decoded: a terminal that reports
/// SHIFT on `+` would turn every typed `+` into a page scroll.
pub(super) fn page_key(key: KeyEvent, native: bool) -> Option<bool> {
    let shift = key.modifiers == KeyModifiers::SHIFT;
    match key.code {
        KeyCode::Char('f') if !native && is_ctrl(key, 'f') => Some(true),
        KeyCode::Char('b') if !native && is_ctrl(key, 'b') => Some(false),
        KeyCode::PageDown if key.modifiers.is_empty() => Some(true),
        KeyCode::PageUp if key.modifiers.is_empty() => Some(false),
        KeyCode::Down | KeyCode::Enter if shift => Some(true),
        KeyCode::Up if shift => Some(false),
        _ => None,
    }
}

/// The Insert-mode page keys (`i_<PageDown>`/`i_<S-Down>` forward, `i_<PageUp>`/`i_<S-Up>` back).
pub(super) fn insert_page_key(key: KeyEvent) -> Option<bool> {
    match key.code {
        KeyCode::PageDown | KeyCode::PageUp | KeyCode::Down | KeyCode::Up => {
            page_key(key, true).filter(|_| key.code != KeyCode::Enter)
        }
        _ => None,
    }
}

/// A window whose top line a scroll command set (nvim's `VALID_TOPLINE`): the frame keeps it while the
/// cursor, the top line and the text stay as the command left them.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct ScrollValid {
    pub(super) window: usize,
    pub(super) cursor: usize,
    pub(super) top: usize,
    pub(super) revision: ruse_core::Revision,
}

/// What a scroll key asks of [`run_scroll`] (#615).
#[derive(Clone, Copy)]
pub(super) enum ScrollKey {
    /// `CTRL-E` (`up`) / `CTRL-Y`, `count` lines.
    Lines { up: bool, count: usize },
    /// `CTRL-F` / `CTRL-B` and the page keys, `count` pages.
    Page { forward: bool, count: usize },
    /// `CTRL-D` / `CTRL-U`, the window's 'scroll' rows.
    Half { forward: bool, amount: usize },
}

/// Run a scroll key on the focused window as nvim v0.12.6 does over its screen rows (`scroll.rs`), then
/// apply the result: the view's top line and skipped columns, and the cursor (placed with the wanted
/// column kept, or moved there when the command set a new one). Returns false when nothing moved (nvim
/// beeps).
#[allow(clippy::too_many_arguments)]
pub(super) fn run_scroll(
    key: ScrollKey,
    ed: &mut Editing,
    frame: &FrameCtx,
    line_idx: &line_index::LineIndex,
    folds: &[crate::folds::Fold],
    so: usize,
    height: usize,
    screen_want: &mut Option<(usize, usize)>,
    valid: &mut Option<ScrollValid>,
) -> bool {
    use crate::scroll::Scroll;
    let Some(win) = frame.wins.get(ed.ws.focus()) else {
        return false;
    };
    let (pos, curswant, mode) = scroll_start(&ed.ws, win, line_idx, *screen_want);
    let (done, start) = {
        let f = ed.ws.focused();
        let text = PaneText {
            bytes: f.doc.bytes(),
            idx: line_idx,
        };
        let lines = PaneLines {
            text,
            win,
            folds,
            virt: &[],
            height,
        };
        let sc = Scroll {
            lines: &lines,
            height,
            so,
            sms: win.smoothscroll,
            tab_end: win.tab_end(mode),
            memo: Default::default(),
        };
        let done = match key {
            ScrollKey::Lines { up, count } => sc.scroll_lines(pos, up, count),
            ScrollKey::Page { forward, count } => sc.page(pos, forward, count, false),
            ScrollKey::Half { forward, amount } => sc.page(pos, forward, amount, true),
        };
        let start = text.start(done.pos.line);
        (done, start)
    };
    let i = ed.ws.focus();
    ed.ws.set_top(i, done.pos.top);
    ed.ws.set_skipcol(i, done.pos.skipcol);
    let at = start + done.pos.byte;
    let cursor = ed.ws.focused().view.cursor().0;
    if at != cursor {
        if done.pos.curswant == curswant {
            // The wanted column stays (nvim's scrollup/scrolldown keep `curswant`).
            place_cursor(&mut ed.ws, &mut ed.recorded, at);
        } else {
            let target = u32::try_from(at + 1).unwrap_or(u32::MAX);
            ed.run(Command::Move(target, ruse_core::Motion::GotoByte));
        }
    }
    *screen_want = Some((ed.ws.focused().view.cursor().0, done.pos.curswant));
    // nvim marks the top line valid after a scroll: the next redraw keeps it (no 'scrolloff' pull back)
    // until the cursor or the text moves.
    *valid = done.keep_top.then(|| ScrollValid {
        window: i,
        cursor: ed.ws.focused().view.cursor().0,
        top: done.pos.top,
        revision: ed.ws.focused().doc.revision(),
    });
    if done.beep {
        ed.ws.beep();
    }
    !done.beep
}

/// Where a scroll key starts in the focused window: its top line, skipped columns (when it wraps) and
/// cursor as a [`crate::scroll::ViewPos`], with the wanted column `screen_want` keeps while the cursor
/// stays where a `gj` / `gk` left it, and the mode.
fn scroll_start(
    ws: &Workspace,
    win: &WinView,
    line_idx: &line_index::LineIndex,
    screen_want: Option<(usize, usize)>,
) -> (crate::scroll::ViewPos, usize, ruse_core::Mode) {
    let f = ws.focused();
    let bytes = f.doc.bytes();
    let text = PaneText {
        bytes,
        idx: line_idx,
    };
    let cursor = f.view.cursor().0.min(bytes.len());
    let line = line_idx.line_of(cursor);
    let curswant = match screen_want {
        Some((at, w)) if at == cursor => w,
        _ => f.view.curswant(),
    };
    (
        crate::scroll::ViewPos {
            top: f.view.top(),
            skipcol: if win.wrap { f.view.skipcol() } else { 0 },
            line,
            byte: cursor - text.start(line),
            curswant,
        },
        curswant,
        f.view.mode(),
    )
}

/// The 0-based buffer line `H` / `M` / `L` (`motion`, one of the viewport [`ScreenMotion`]s) with the
/// effective `count` lands on in the focused window, keeping `scrolloff` rows from its edges. Shared by the
/// `H` / `M` / `L` key and by `.` of a change over one, which resolves the line again (#1577).
///
/// [`ScreenMotion`]: crate::input::ScreenMotion
#[allow(clippy::too_many_arguments)]
pub(super) fn viewport_line(
    motion: crate::input::ScreenMotion,
    count: u32,
    scrolloff: usize,
    ws: &Workspace,
    frame: &FrameCtx,
    line_idx: &line_index::LineIndex,
    folds: &[crate::folds::Fold],
    focused_h: usize,
) -> usize {
    use super::viewport;
    use crate::input::ScreenMotion;
    let to = match motion {
        ScreenMotion::ViewHigh => viewport::ScreenTo::High,
        ScreenMotion::ViewMiddle => viewport::ScreenTo::Middle,
        _ => viewport::ScreenTo::Low,
    };
    let snapshot = &frame.snapshot;
    // The last real line: the slot after a final newline is no line (#677), so `M` on a short
    // buffer takes the middle of its lines, not of one more.
    let last_line = line_idx
        .line_of(ruse_core::motion::off_phantom(snapshot, ruse_core::BytePos(snapshot.len())).0);
    // The lines that show whole: fewer than the rows when lines wrap (#615).
    let top = ws.focused().view.top();
    let shown = match frame.wins.get(ws.focus()) {
        Some(win) => {
            let lines = PaneLines {
                text: PaneText {
                    bytes: snapshot,
                    idx: line_idx,
                },
                win,
                folds,
                virt: &[],
                height: focused_h,
            };
            if folds.iter().any(|f| f.closed) {
                // A closed fold is one screen line (#1141): pick among the lines shown.
                let (vis, at_end) = viewport::shown_lines(&lines, top, focused_h);
                // Indices into `vis`, offset by one when the window does not start at the first
                // line, so `H` keeps its 'scrolloff' margin there.
                let t0 = usize::from(top > 0);
                let last = t0 + if at_end { vis.len() - 1 } else { vis.len() };
                let i = viewport::screen_line(t0, vis.len(), scrolloff, to, count, last);
                Err(vis[(i - t0).min(vis.len() - 1)])
            } else {
                let (bot, _) = viewport::botline(&lines, top, focused_h);
                Ok(if bot >= viewport::Lines::count(&lines) {
                    focused_h
                } else {
                    (bot - top).max(1)
                })
            }
        }
        None => Ok(focused_h),
    };
    match shown {
        Ok(shown) => viewport::screen_line(top, shown, scrolloff, to, count, last_line),
        Err(row) => row,
    }
}

/// Resolve a screen-line motion (`gj`, `gk`, `g0`, `g^`, `gm`, `g$`; #615) in the focused window and let
/// the engine finish it: the bare move, or its armed operator over it. `want` carries nvim's wanted
/// column from one `gj` / `gk` to the next while the cursor stays where it left it (the core's own wanted
/// column does not count the window's rows).
#[allow(clippy::too_many_arguments)]
pub(super) fn resolve_screen_motion(
    motion: crate::input::ScreenMotion,
    count: u32,
    ws: &Workspace,
    engine: &mut InputEngine,
    frame: &FrameCtx,
    line_idx: &line_index::LineIndex,
    folds: &[crate::folds::Fold],
    want: &mut Option<(usize, usize)>,
) -> Feed {
    use crate::ui::window_view::{screen_motion, Resolved};
    let Some(win) = frame.wins.get(ws.focus()) else {
        return engine.screen_motion_fail();
    };
    let view = &ws.focused().view;
    let bytes = ws.focused().doc.bytes();
    let text = PaneText {
        bytes,
        idx: line_idx,
    };
    let cursor = view.cursor().0.min(bytes.len());
    let line = line_idx.line_of(cursor);
    let curswant = match *want {
        Some((at, w)) if at == cursor => w,
        _ => view.curswant(),
    };
    let resolved = screen_motion(
        text,
        win,
        folds,
        line,
        cursor - text.start(line),
        curswant,
        motion,
        count as usize,
        matches!(view.mode(), ruse_core::Mode::Visual { .. }),
    );
    let op = engine.has_op();
    match resolved {
        Resolved::Lines { down } => {
            *want = None;
            engine.screen_motion_lines(down)
        }
        Resolved::Fail => engine.screen_motion_fail(),
        // nvim moves as far as it can and beeps; an operator is dropped.
        Resolved::At {
            complete: false, ..
        } if op => engine.screen_motion_fail(),
        // `gj` after `$` keeps the end-of-line wanted column (nvim's MAXCOL); landing on a line's last char
        // below, that is `{n}$`, which keeps it in the core too.
        Resolved::At { landing, .. }
            if landing.want == Some(usize::MAX)
                && landing.line >= line
                && !(line..=landing.line)
                    .any(|l| crate::folds::closed_range(folds, l).is_some())
                && ruse_core::cells::last_glyph(text.line(landing.line))
                    .is_none_or(|i| i == landing.byte) =>
        {
            *want = None;
            engine
                .screen_motion_line_end(u32::try_from(landing.line - line + 1).unwrap_or(u32::MAX))
        }
        Resolved::At { landing, .. } => {
            let start = text.start(landing.line);
            let mut byte = start + landing.byte;
            if op && landing.inclusive {
                // An inclusive motion under an operator reaches the byte after its char.
                let line_text = text.line(landing.line);
                byte = start
                    + ruse_core::cells::glyphs(line_text)
                        .map(|(i, g)| i + g.len())
                        .find(|&e| e > landing.byte)
                        .unwrap_or(line_text.len());
            }
            *want = match (op, landing.want) {
                (false, Some(w)) => Some((byte, w)),
                _ => None,
            };
            engine.screen_motion_to(byte)
        }
    }
}
