//! The event-loop hub (#878). [`Session`] owns the editor state; `run` owns the terminal and drives the
//! `while !quit` loop: [`Session::pre_frame`] snapshots the focused buffer and scrolls each pane, `run`
//! highlights the viewport and renders the frame, [`Session::post_render`] applies deferred LSP results,
//! then `run` reads one key and [`Session::on_key`] routes it (macro layer / confirm loop / overlays /
//! session prefixes / the input engine). `crate::headless` drives the same methods without a terminal.

use std::collections::{HashMap, HashSet};
use std::io;
use std::path::PathBuf;

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::terminal as ct_terminal;

use ruse_core::{
    BytePos, CaretGravity, Command, DocumentId, EditorOption, ExCmd, ExOutcome, GroupKind, Mode,
    OpenKind, SplitDir, Trace, TransactionOrigin, Workspace,
};

use crate::app::dispatch::{
    attach_file, buffer_list_line, dest_in_range, focused_buffer_label, is_ctrl, lines_outcome,
    match_count_message, may_quit, note_ex, open_file_into_buffer, place_cursor, quit_window,
    record_register_writes, regex_error_msg, run_cmd, run_ex, run_repeat, sub_outcome,
    substitute_cmd, write_register, Files, Highlighters,
};
use crate::app::ex_range::ExRange;
use crate::app::lsp_coordinator::LspCoordinator;
use crate::app::messages::{Msg, Silence, Status};
use crate::app::report;
use crate::input::{
    parse_ex, BufTarget, EditSpec, Ex, Feed, InputEngine, Pending, ReadSource, WriteSpec,
    WriteTarget,
};
use crate::terminal::guard::TermGuard;
use crate::ui::overlay::{Accepted, Overlay};
use crate::ui::palette::focused_context;
use crate::ui::picker::PickOutcome;
use crate::ui::prompts::{confirm_key, confirm_prompt, Confirm};
use crate::ui::render::{render, search_pattern};
use crate::ui::window_view::{HScroll, PaneLines, PaneText, WinView};
use crate::ui::{
    action_picker, buffer_picker, diag_picker, file_picker,
    layout::{window_rects, Rect},
    line_picker, marks_picker, palette, pos_picker, ref_picker, register_picker,
};
use crate::{graphics, health, highlight, indent, line_index, persist, recover, screen, viewport};
#[cfg(unix)]
use crate::{pty, term_buffer};

/// The rows of context a window keeps (`'scrolloff'`, #1151): its own value when it has one that is not
/// negative (nvim's `get_scrolloff_value`), else the global one; a negative global keeps none.
fn scrolloff_for(view: &ruse_core::View, global: i64) -> usize {
    let so = view.scrolloff_local().filter(|&n| n >= 0).unwrap_or(global);
    usize::try_from(so).unwrap_or(0)
}

/// Rows of context kept above and below the cursor when scrolling (Vim's `scrolloff`): ruse's default
/// (`editor.scrolloff`), until `:set so` changes [`Session::scrolloff`].
const SCROLLOFF: i64 = 3;

/// How deep `:normal` may nest (`:normal :normal …`, or a macro that runs `:normal @a` on itself) before it
/// fails with E192. Vim's limit is 'maxmapdepth' (1000); each level here is a few session frames on the
/// Rust stack, so ruse stops earlier.
const MAX_NORMAL_DEPTH: usize = 20;

/// Where Insert resumes after a `:normal` run from `i_CTRL-O` (#1097): the caret's line, and whether it
/// sat past the end of that line.
struct InsertReturn {
    line: usize,
    at_eol: bool,
}

/// Give a command's line-count report (#1276), or clear the line when it has none ('report' not passed).
fn give_report(status: &mut Status, report: Option<String>) {
    match report {
        Some(msg) => status.info(msg),
        None => status.clear(),
    }
}

fn to_i64(n: usize) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

/// `{count}&`: repeat the last `:s` on the cursor's line and the `count - 1` after it, KEEPING its flags.
/// nvim's default mapping is `nnoremap & :&&<CR>` (`:help &-default`), so `{count}&` is
/// `:.,.+{count-1}&&` and `j&` after `:s/a/b/g` replaces every match. A range past the last line is E16,
/// a line without a match E486, and nothing to repeat E33, as there. This is the only `&` path: the typed
/// key and the `RepeatSubstituteLine` command both come here.
fn repeat_substitute_lines(
    ws: &mut Workspace,
    recorded: &mut Vec<Command>,
    last: Option<&(String, String, ruse_core::SubFlags)>,
    count: usize,
    report: i64,
) -> Msg {
    let Some((pat, rep, flags)) = last else {
        return Msg::error("E33: No previous substitute regular expression");
    };
    let bytes = ws.focused().doc.bytes();
    let first = ruse_core::pos::line_of(bytes, ws.focused().view.cursor()) + 1;
    let lines = bytes.split(|b| *b == b'\n').count() - usize::from(bytes.ends_with(b"\n"));
    let last = first + count - 1;
    if last > lines.max(1) {
        return Msg::error("E16: Invalid range");
    }
    // Recorded as the resolved `:s` it repeats (RFC-0020); a match is a jump, as `:s` is (#1119).
    let range = ruse_core::SubRange::Lines(first, last);
    let ex = substitute_cmd(ws, range, pat, rep, *flags, None);
    match sub_outcome(run_ex(ex, ws, recorded)) {
        Ok(out) if out.replacements == 0 => Msg::error(format!("E486: Pattern not found: {pat}")),
        Ok(out) => crate::app::dispatch::substituted_message(&out, report),
        Err(e) => Msg::error(crate::app::dispatch::regex_error_msg(&e)),
    }
}

/// `:r`: insert `bytes` (what the frontend read from the file or the command) below `addr`, recorded with
/// the bytes as data so a replay never reads or runs anything (RFC-0020 R2). Returns the lines inserted; an
/// address off the buffer inserts nothing.
fn read_text(
    ws: &mut Workspace,
    recorded: &mut Vec<Command>,
    addr: ruse_core::LineAddr,
    bytes: Vec<u8>,
    cursor_on_last: bool,
) -> usize {
    ws.resolve_addr(addr).map_or(0, |dest| {
        let ex = ExCmd::ReadText {
            dest,
            cursor_on_last,
            bytes,
        };
        lines_outcome(&run_ex(ex, ws, recorded))
    })
}

/// The start of the UTF-8 char before byte `at` (`0` at the start).
fn prev_char_start(b: &[u8], at: usize) -> usize {
    let mut i = at.saturating_sub(1);
    while i > 0 && b.get(i).is_some_and(|&c| c & 0xC0 == 0x80) {
        i -= 1;
    }
    i
}

/// The start of the UTF-8 char after the one at byte `at`.
fn next_char_start(b: &[u8], at: usize) -> usize {
    let mut i = at + 1;
    while b.get(i).is_some_and(|&c| c & 0xC0 == 0x80) {
        i += 1;
    }
    i
}

/// How often the event loop repaints while a macro replay runs (see `run`).
const REPLAY_PAINT: std::time::Duration = std::time::Duration::from_millis(100);

/// F-011: when a terminal is live the event loop polls for a key with this timeout (≈30fps) instead of
/// blocking, so asynchronous PTY output renders within one tick. Only the terminal-active path polls.
#[cfg(unix)]
const TERM_TICK_MS: u64 = 33;

/// F-014: with only a language server live (no terminal), poll more slowly — diagnostics arrive infrequently,
/// so a ~10fps tick keeps them prompt without spinning.
const LSP_TICK_MS: u64 = 100;

/// The session-level key prefix awaiting its next key (#878). One value instead of one flag per prefix, so
/// two prefixes can never be armed at once.
enum Prefix {
    /// No session prefix is armed.
    None,
    /// `C-w` (Normal mode): the next key is a window command (F-007). Holds the count typed before `C-w` or
    /// between it and the command (0 = none): `{count}C-w w` goes to window `count` (#1068).
    Window(u32),
    /// `z` (Normal or Visual): the next key is a scroll or fold verb (`zz`/`zt`/`zb`/`zf`/`zo`…). Holds the
    /// count typed before `z` (0 = none): `{count}zt` puts line `count` at the top (#850).
    Z(u32),
    /// `zf` in Normal mode awaits a {motion}: the keys typed after `zf` (an optional count and a linewise
    /// motion, e.g. `3j`/`gg`/`ip`), buffered until they parse to a range for the reindent seam. Empty just
    /// after `zf`.
    Zf(String),
    /// A `z` or `C-w` typed under an armed operator cancelled it (Vim's `checkclearop`); the key it would
    /// have taken next is swallowed without effect, as in Vim.
    Swallow,
    /// Terminal mode `CTRL-\` awaits `CTRL-N`, the Terminal → Terminal-Normal escape (F-011).
    #[cfg(unix)]
    TermEscape,
}

/// The page-scroll direction a Normal/Visual key names (`true` = forward), nvim v0.12.6 `:help CTRL-F`:
/// `C-f`/`<PageDown>`/`<S-Down>`/`<S-CR>` forward, `C-b`/`<PageUp>`/`<S-Up>` back. `C-f`/`C-b` are left to
/// the Native profile's pickers when `native`. `<S-+>`/`<S-NL>` are not decoded: a terminal that reports
/// SHIFT on `+` would turn every typed `+` into a page scroll.
fn page_key(key: KeyEvent, native: bool) -> Option<bool> {
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
fn insert_page_key(key: KeyEvent) -> Option<bool> {
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
    window: usize,
    cursor: usize,
    top: usize,
    revision: ruse_core::Revision,
}

/// What a scroll key asks of [`run_scroll`] (#615).
#[derive(Clone, Copy)]
enum ScrollKey {
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
fn run_scroll(
    key: ScrollKey,
    ws: &mut Workspace,
    frame: &FrameCtx,
    line_idx: &line_index::LineIndex,
    folds: &[crate::folds::Fold],
    so: usize,
    height: usize,
    screen_want: &mut Option<(usize, usize)>,
    valid: &mut Option<ScrollValid>,
    files: &Files,
    recorded: &mut Vec<Command>,
    status: &mut Status,
    quit: &mut bool,
) -> bool {
    use crate::scroll::{Scroll, ViewPos};
    let Some(win) = frame.wins.get(ws.focus()) else {
        return false;
    };
    let (pos, curswant, mode) = {
        let f = ws.focused();
        let bytes = f.doc.bytes();
        let text = PaneText {
            bytes,
            idx: line_idx,
        };
        let cursor = f.view.cursor().0.min(bytes.len());
        let line = line_idx.line_of(cursor);
        let curswant = match *screen_want {
            Some((at, w)) if at == cursor => w,
            _ => f.view.curswant(),
        };
        (
            ViewPos {
                top: f.view.top(),
                skipcol: if win.wrap { f.view.skipcol() } else { 0 },
                line,
                byte: cursor - text.start(line),
                curswant,
            },
            curswant,
            f.view.mode(),
        )
    };
    let (done, start) = {
        let f = ws.focused();
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
    let i = ws.focus();
    ws.set_top(i, done.pos.top);
    ws.set_skipcol(i, done.pos.skipcol);
    let at = start + done.pos.byte;
    let cursor = ws.focused().view.cursor().0;
    if at != cursor {
        if done.pos.curswant == curswant {
            // The wanted column stays (nvim's scrollup/scrolldown keep `curswant`).
            place_cursor(ws, recorded, at);
        } else {
            let target = u32::try_from(at + 1).unwrap_or(u32::MAX);
            run_cmd(
                Command::Move(target, ruse_core::Motion::GotoByte),
                ws,
                files,
                recorded,
                status,
                quit,
            );
        }
    }
    *screen_want = Some((ws.focused().view.cursor().0, done.pos.curswant));
    // nvim marks the top line valid after a scroll: the next redraw keeps it (no 'scrolloff' pull back)
    // until the cursor or the text moves.
    *valid = done.keep_top.then(|| ScrollValid {
        window: i,
        cursor: ws.focused().view.cursor().0,
        top: done.pos.top,
        revision: ws.focused().doc.revision(),
    });
    if done.beep {
        ws.beep();
    }
    !done.beep
}

/// The union of the byte ranges visible in every pane showing the FOCUSED doc — the window syntax
/// highlighting is bounded to, so a per-keystroke reparse stays O(viewport). Falls back to the whole buffer
/// when no pane matches. Pure over the layout + revision-cached line index (lifted out of the `run` loop).
fn visible_byte_range(
    ws: &Workspace,
    rects: &[Rect],
    line_idx: &line_index::LineIndex,
    buf_len: usize,
) -> std::ops::Range<usize> {
    let focus_doc = ws.focused().view.doc();
    let (mut vis_start, mut vis_end) = (usize::MAX, 0usize);
    for (i, rect) in rects.iter().enumerate() {
        let p = ws.pane(i);
        if p.view.doc() != focus_doc {
            continue;
        }
        vis_start = vis_start.min(line_idx.nth_line_start(p.view.top()));
        vis_end = vis_end.max(line_idx.nth_line_start(p.view.top() + rect.h as usize + 1));
    }
    if vis_start <= vis_end {
        vis_start..vis_end
    } else {
        0..buf_len
    }
}

/// Resolve a screen-line motion (`gj`, `gk`, `g0`, `g^`, `gm`, `g$`; #615) in the focused window and let
/// the engine finish it: the bare move, or its armed operator over it. `want` carries nvim's wanted
/// column from one `gj` / `gk` to the next while the cursor stays where it left it (the core's own wanted
/// column does not count the window's rows).
#[allow(clippy::too_many_arguments)]
fn resolve_screen_motion(
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

/// Read each on-screen virtual image's pixel dimensions from its PNG IHDR (the 24-byte header only), so the
/// renderer can size it to its natural aspect. Cheap and only when the terminal is graphics-capable; a
/// missing/invalid file is silently skipped. Pure over the virt-line set (lifted out of the `run` loop).
fn read_image_dims(
    has_graphics: bool,
    virt_lines: &[highlight::VirtLine],
) -> HashMap<String, (u32, u32)> {
    if !has_graphics {
        return HashMap::new();
    }
    use std::io::Read;
    virt_lines
        .iter()
        .filter_map(|v| {
            let p = v.path.as_ref()?;
            let mut hdr = [0u8; 24];
            std::fs::File::open(p).ok()?.read_exact(&mut hdr).ok()?;
            graphics::png_dimensions(&hdr).map(|d| (p.clone(), d))
        })
        .collect()
}

/// Rewrite a command into its tree-aware variant when the focused buffer has a live syntax tree (F-015).
/// Three independent rewrites, each a no-op without a tree so plain-text editing is untouched:
/// - `Reindent` (`=`) → `SetIndents` with tree-computed levels (else the core bracket-depth fallback).
/// - `OpenBelow`/`OpenAbove`/`InsertNewline` (`o`/`O`/`<CR>`) → `OpenLineIndent` seeded with the suggested
///   indent for the newly opened line.
/// - `InsertChar` of a closer `}`/`)`/`]` → `InsertCloser`, which auto-dedents to the matching opener.
///
/// Pure `Command -> Command` over the workspace + highlighter cache (lifted out of the `run` loop).
fn resolve_tree_aware(
    cmd: Command,
    ws: &Workspace,
    highlighters: &HashMap<DocumentId, highlight::CachedHighlight>,
    line_idx: &line_index::LineIndex,
    snapshot: &[u8],
) -> Command {
    // A buffer whose language is not indented by node depth (Markdown, #818) keeps the core's plain `=`/`o`.
    let has_tree = |ws: &Workspace| {
        highlighters
            .get(&ws.focused_buffer())
            .and_then(highlight::CachedHighlight::tree)
            .filter(|t| indent::uses_node_depth(t))
    };
    // Lines `first..=last` indent by the tree, or, where it has an error, by a window of the buffer with its
    // open blocks closed, so a block still being typed indents by the depth it will have (#1214).
    let tree_of = |ws: &Workspace, first: usize, last: usize| {
        has_tree(ws).map(|t| {
            let next = line_idx.nth_line_start(last + 1);
            let last_le = if next > 0 && snapshot.get(next - 1) == Some(&b'\n') {
                next - 1
            } else {
                snapshot.len()
            };
            indent::IndentTree::new(t, snapshot, first, line_idx.nth_line_start(first), last_le)
        })
    };
    // `'lisp'` indents in the core, as nvim's Lisp rules win over an indent expression (#1163).
    if ws.focused().view.lisp() {
        return cmd;
    }
    // C indents by nvim's cindent after the edit (`indent_keys`), and `=` in the core (#1163).
    if crate::app::indent_keys::is_cindent(ws, highlighters) {
        return match cmd {
            Command::InsertReindent => crate::app::indent_keys::c_reindent_caret_line(ws, snapshot),
            other => other,
        };
    }
    // Python indents as nvim's indent script does, after the edit (`indent_keys`), in columns (#1205).
    let python = crate::app::indent_keys::is_python(ws, highlighters);
    if python {
        return match cmd {
            Command::Reindent { count, motion } => match ws.reindent_range(motion, count) {
                Some((first, last)) => {
                    crate::app::indent_keys::reindent(ws, highlighters, snapshot, first, last)
                }
                None => Command::Reindent { count, motion },
            },
            Command::InsertReindent => {
                crate::app::indent_keys::reindent_caret_line(ws, highlighters, snapshot)
            }
            other => other,
        };
    }
    // `i_CTRL-F` re-indents the caret's line from the tree.
    if matches!(cmd, Command::InsertReindent) {
        let row = line_idx.line_of(ws.focused().view.cursor().0);
        if let Some(tree) = tree_of(ws, row, row) {
            return Command::SetIndents {
                first_line: row,
                last_line: row,
                levels: tree.levels(row, row),
            };
        }
        return cmd;
    }
    // `=` — resolve the reindent range + levels from the tree, else fall through to the core `=`.
    let cmd = if let Command::Reindent { count, motion } = cmd {
        let range = ws.reindent_range(motion, count);
        match range.and_then(|(first, last)| Some((first, last, tree_of(ws, first, last)?))) {
            Some((first_line, last_line, tree)) => Command::SetIndents {
                first_line,
                last_line,
                levels: tree.levels(first_line, last_line),
            },
            None => Command::Reindent { count, motion },
        }
    } else {
        cmd
    };
    // `o`/`O`/`<CR>` — seed the opened line with the tree-suggested indent. Dot-repeat replays the plain
    // open (column-0), so a stale recorded level is never re-applied at a new location.
    let continues_comment = {
        let pane = ws.focused();
        pane.view.opens_comment(pane.doc.bytes(), &cmd)
    };
    let cmd = if matches!(
        cmd,
        Command::OpenBelow | Command::OpenAbove | Command::InsertNewline
    ) && !continues_comment
    {
        let cur = ws.focused().view.cursor().0;
        let row = line_idx.line_of(cur);
        match tree_of(ws, row, row) {
            Some(tree) => {
                let ls = line_idx.nth_line_start(row);
                let next = line_idx.nth_line_start(row + 1);
                let le = if next > 0 && snapshot.get(next - 1) == Some(&b'\n') {
                    next - 1
                } else {
                    snapshot.len()
                };
                // Where the tree is still broken after closing the open blocks, its depth means nothing:
                // the core's plain open keeps the line's indent (autoindent, #1214).
                let at = match cmd {
                    Command::OpenBelow => le,
                    Command::OpenAbove => ls,
                    _ => cur,
                };
                if tree.broken_at(at) {
                    return cmd;
                }
                let (kind, level) = match cmd {
                    Command::OpenBelow => (OpenKind::Below, tree.open_below(ls, le, row + 1)),
                    Command::OpenAbove => (OpenKind::Above, tree.suggest_indent(ls, row)),
                    _ => (OpenKind::Split, tree.suggest_indent(cur, row + 1)),
                };
                Command::OpenLineIndent { kind, level }
            }
            None => cmd,
        }
    } else {
        cmd
    };
    // `}`/`)`/`]` — realign to the matching opener in a tree-backed buffer (the core bracket-match decides
    // whether to actually move). Dot-repeat replays the plain `InsertChar` (column unchanged).
    match cmd {
        Command::InsertChar(c @ ('}' | ')' | ']')) if has_tree(ws).is_some() => {
            Command::InsertCloser { ch: c }
        }
        other => other,
    }
}

/// The status line after a `:r !cmd` / filter: nvim shows `shell returned {n}` ahead of the line count when
/// the command failed (#861).
fn with_shell_failure(out: &crate::shell::ShellOutput, lines: String) -> String {
    match (out.failure(), lines.is_empty()) {
        (Some(note), true) => note,
        (Some(note), false) => format!("{note}; {lines}"),
        (None, _) => lines,
    }
}

/// The terminal capabilities the session reports in `:checkhealth` (F-030), copied out of the `TermGuard`
/// at startup so the key path never needs the guard itself.
#[derive(Clone, Copy, Default)]
pub(crate) struct TermInfo {
    pub(crate) sgr_mouse: bool,
    pub(crate) sync_output: bool,
    pub(crate) bracketed_paste: bool,
    pub(crate) focus_events: bool,
}

/// The values one frame computes before its key is read and the key's handlers read back (#878): the
/// focused buffer's revision and byte snapshot (taken before any `&mut ws` edit of this key) and the pane
/// geometry. Built by [`Session::pre_frame`], so every key — a macro-replayed one too — sees a fresh frame.
pub(crate) struct FrameCtx {
    pub(crate) revision: ruse_core::Revision,
    pub(crate) snapshot: std::sync::Arc<Vec<u8>>,
    pub(crate) rects: Vec<Rect>,
    /// Each window's display for the frame (wrap, gutter, horizontal scroll; #615, #616).
    pub(crate) wins: Vec<WinView>,
    pub(crate) cols: u16,
    pub(crate) text_rows: u16,
}

/// What the caller's loop does after a key: keep going, or end the session.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Flow {
    Continue,
    Quit,
}

/// How a session is seeded: the start buffer and its file, the profile, and the environment it runs in.
pub(crate) struct SessionInit {
    /// The session file, if any (named buffer, `:w` target, journal key).
    pub(crate) path: Option<PathBuf>,
    pub(crate) fmt: persist::encoding::FileFormat,
    /// The start buffer's text (the disk text, or the recovered text).
    pub(crate) initial: Vec<u8>,
    pub(crate) status: String,
    /// `"vim"`, `"emacs"` or `"native"`.
    pub(crate) profile: &'static str,
    pub(crate) term: TermInfo,
    pub(crate) cwd: PathBuf,
    pub(crate) trusted: bool,
    /// Publish every recoverable buffer to the crash-rescue store and journal file buffers each frame. The
    /// terminal session does; a headless test session does not touch that process-global state.
    pub(crate) rescue: bool,
}

/// Put the cursor on 1-based `line` for an Ex range: Vim `beginline(BL_SOL | BL_FIX)` under
/// 'nostartofline' keeps the wanted column, so this is `j`/`k` by the distance (nvim v0.12.6 `ll:3` stays
/// in column 2).
fn goto_line(
    line: usize,
    ws: &mut Workspace,
    files: &Files,
    recorded: &mut Vec<Command>,
    status: &mut Status,
    quit: &mut bool,
) {
    let b = ws.focused().doc.bytes();
    let here = ruse_core::pos::line_of(b, ws.focused().view.cursor()) + 1;
    let (n, motion) = if line >= here {
        (line - here, ruse_core::Motion::Down)
    } else {
        (here - line, ruse_core::Motion::Up)
    };
    if n > 0 {
        let cmd = Command::Move(n as u32, motion);
        crate::app::dispatch::run_cmd(cmd, ws, files, recorded, status, quit);
    }
}

/// A `:` line stopped at a backwards range (#1093): the command with its range swapped, then the
/// commands after it on the line, run in order once the prompt is answered `y`.
struct SwapPrompt {
    pending: Vec<String>,
}

/// The editor state the event loop owns (#878): everything `run` used to keep in locals except the
/// terminal (guard, output, previous frame, graphics placements). `run` drives it with
/// [`Session::pre_frame`] → render → [`Session::post_render`] → read a key → [`Session::on_key`]; the
/// headless harness (`crate::headless`) drives the same methods without a terminal.
pub(crate) struct Session {
    pub(crate) ws: Workspace,
    pub(crate) files: Files,
    pub(crate) highlighters: HashMap<DocumentId, highlight::CachedHighlight>,
    /// Revision-cached line index: O(log n) row/line lookups instead of an O(buffer) newline scan per
    /// pane per frame (D-042 win D). Refreshed by `pre_frame`.
    pub(crate) line_idx: line_index::LineIndex,
    /// The line indexes of the other buffers the windows show (#1182), refreshed by `pre_frame`.
    pub(crate) pane_idx: line_index::PaneIndexes,
    pub(crate) initial: Vec<u8>,
    pub(crate) recorded: Vec<Command>,
    /// The editing options the core reads, as the session began: a saved trace starts by setting them, so
    /// it replays the same under another config (RFC-0020 R4).
    pub(crate) start_options: Vec<EditorOption>,
    /// The registers that held a value as the session began (RFC-0020 R6): a saved trace writes them after
    /// the starting options, so a `p` of one replays the same text. Read with the starting options.
    pub(crate) start_registers: Vec<(Option<char>, ruse_core::Register)>,
    /// What the session did to the trace's document that the trace cannot record (#690): `:trace save`
    /// refuses a trace that would not replay to it.
    pub(crate) trace_watch: crate::app::trace_check::TraceWatch,
    /// The journal directory, when this session journals (`rescue`) and a state directory exists.
    journals: Option<&'static persist::journal::Journals>,
    /// Per-buffer journal writers (RFC-0023 §4): each drains its document's change log after every command
    /// and syncs on idle, on a change count, and before a risky operation.
    journal_writers: HashMap<DocumentId, persist::journal::Writer>,
    /// Buffers whose last journal write failed, so the E303 warning shows once per failing run (#907).
    journal_failed: HashSet<DocumentId>,
    /// File buffers whose file on disk differs from what ruse last read or saved (`[disk changed]`,
    /// RFC-0023 §8), as of the last [`Session::check_disk`].
    disk_changed: HashSet<DocumentId>,
    /// The file buffers W11/W12 (changed) or E211 (gone) was shown for, and for which state: once per
    /// change.
    disk_warned: HashMap<DocumentId, persist::disk::State>,
    /// The last key or paste, and the last disk check: the idle check runs [`persist::journal::IDLE`]
    /// after both.
    last_input: std::time::Instant,
    disk_checked: std::time::Instant,
    /// `'readonly'` beyond a file buffer's own flag: W10, `ruse -R`, buffers without a file (#1211).
    pub(crate) readonly: crate::app::dispatch::ReadOnly,
    /// `ruse` with no file: the start buffer has no path, but a crash rescues it into this process's unnamed
    /// journal, which the next pathless start offers (#856). Only the crash paths write it: the buffer can
    /// never be saved, so a periodic journal would outlive every `:q!`.
    unnamed_start: Option<DocumentId>,
    rescue: bool,
    pub(crate) profile_name: &'static str,
    pub(crate) emacs_profile: bool,
    pub(crate) engine: InputEngine,
    pub(crate) quit: bool,
    /// The rows of a message shown above the hit-enter or `-- More --` prompt (#1222, #1235), and the
    /// prompt the last frame showed: the next key answers it.
    pager: crate::ui::message::Pager,
    term: TermInfo,
    /// The armed session key prefix, if any (`C-w`, `z`, `zf`, `[`/`]`, `CTRL-\`).
    prefix: Prefix,
    /// Folds, keyed by buffer (per-buffer, not per-window). Closed folds collapse in render; the cursor/scroll
    /// skip them. Under `foldmethod=manual` (default) edits shift/drop them; under `foldmethod=indent` (slice 3)
    /// they are RECOMPUTED from indentation on edit.
    pub(crate) folds: HashMap<DocumentId, Vec<crate::folds::Fold>>,
    /// The line anchors that carry each buffer's manual folds through its edits (#819).
    fold_anchors: HashMap<DocumentId, crate::folds::Anchored>,
    /// Per-buffer `foldmethod` (default manual), `foldlevel` (default 0), `foldenable` (default true), and the
    /// buffer revision the indent folds were last computed at (recompute when it changes). All frontend state
    /// (folds never leak into core; INV-DOC-VIEW).
    fold_method: HashMap<DocumentId, crate::input::FoldMethod>,
    foldlevel: HashMap<DocumentId, usize>,
    foldenable: HashMap<DocumentId, bool>,
    fold_rev: HashMap<DocumentId, ruse_core::Revision>,
    /// The key half of a Vim-profile Alt chord, delivered on the next turn after its `<Esc>` (#896).
    meta_tail: Option<KeyEvent>,
    /// Vim macros (D-055): `q{a-z}` records the raw keystroke stream into a register, `@{a-z}` replays it.
    /// The whole record/replay state machine lives in `keys::MacroState` (unit-tested end to end).
    macros: crate::keys::MacroState,
    /// The buffer whose undo block a running macro replay holds open (#939): the keys of one `@{reg}` are
    /// not typed, so Vim records all their changes as one undo step. Closed once the replay drains.
    macro_undo: Option<DocumentId>,
    /// A `:s///c` interactive confirm loop, when active (F-009).
    pub(crate) confirm: Option<Confirm>,
    /// A backwards Ex range waiting for its `y`/`n` (`Backwards range given, OK to swap (y/n)?`, #1093).
    swap: Option<SwapPrompt>,
    /// The key that answered [`Session::swap`], handed from the key routing to [`Session::answer_swap`].
    swap_answer: Option<KeyEvent>,
    /// The hlsearch pattern (last `/`-search), until `:noh`.
    pub(crate) search_hl: Option<String>,
    /// hlsearch/incsearch render preferences (`:set (no)hlsearch/incsearch`); ON by default (better-than-Vim,
    /// which defaults hlsearch off — a slow-terminal-era default; matches are viewport-cached here).
    pub(crate) hlsearch_on: bool,
    pub(crate) incsearch_on: bool,
    /// `:set (no)fixeol` write preference; OFF by default — byte-preserve (no silent fixeol) is the honest
    /// default. When ON, `:w` ADDS a trailing `\n` if the buffer lacks one (Vim's fixendofline).
    fixeol_on: bool,
    /// The global `'scrolloff'` as set (nvim keeps any number; a negative one keeps no context here). A
    /// window's own value (`:setlocal so`) is on its View and wins (#1151).
    scrolloff: i64,
    /// `'showmode'` and `'verbose'` (#1151).
    pub(crate) ui_opts: crate::app::set_exec::UiOptions,
    /// nvim's wanted column after a `gj` / `gk` (#615), as `(cursor byte, column)`: the next one reuses it
    /// while the cursor is still there.
    screen_want: Option<(usize, usize)>,
    /// The window whose top line the last scroll command set, kept by the next frames (#615).
    scroll_valid: Option<ScrollValid>,
    /// The `keymap.lang` pairs the config installed, so a reload can take them back (#1151).
    config_lmaps: Vec<(char, char)>,
    /// Where each option's value came from, for `:verbose set opt?` (#614).
    opt_sources: HashMap<crate::input::set_cmd::Opt, crate::app::set_exec::OptSource>,
    /// The command being run came after `:verbose`.
    ex_verbose: bool,
    /// The loaded config layers (#614), for `:inspect config`; empty in a headless session.
    pub(crate) config: crate::config::Config,
    /// Where the config layers and trust records live, when the session loads config (the binary does,
    /// the headless harness does not), so `:trust` / `:untrust` can reload the workspace layer.
    pub(crate) config_paths: Option<crate::config::ConfigPaths>,
    /// The layer files as last loaded: a different stamp reloads the config (#1152).
    config_stamp: crate::config::Stamp,
    /// The crash-loop record (CFG-SAFE-001): `:config-reload` clears it when it leaves safe mode.
    pub(crate) boot_record: Option<crate::config::boot::BootRecord>,
    /// The last `:s` (pattern, replacement, flags) — recorded on every substitute so `&` can repeat it (F-009).
    last_substitute: Option<(String, String, ruse_core::SubFlags)>,
    /// The last executed Ex command-line (WITHOUT the leading `:`) — Vim's `":` register (`:help quote_:`),
    /// read by `C-r :`. Recorded on every `Feed::ExecuteEx`.
    last_ex_line: Option<String>,
    /// nvim's `can_cindent` for the Python 'indentkeys' (#1206): a blank, `<Tab>` or `<BS>` typed in the
    /// indent stops the keys re-indenting the line until the next line or Insert.
    can_cindent: bool,
    /// The last shell command run by `:!`, `:{range}!` or `:r !` (after expansion) — what a `!` in the next
    /// one expands to, so `:!!` repeats it (`:help :!`).
    last_shell: Option<String>,
    /// The open modal picker overlay (F-004 / F-013 NAT-3): command palette, line jump, buffer switch, file
    /// finder, the LSP pickers and the view-only listings. One slot, so at most one is open (#879).
    pub(crate) overlay: Option<Overlay>,
    /// F-011: live terminal buffers keyed by their placeholder DocumentId (unix-only). `Prefix::TermEscape`
    /// tracks a `CTRL-\` awaiting `CTRL-N` (the Terminal → Terminal-Normal escape).
    #[cfg(unix)]
    pub(crate) terminals: HashMap<DocumentId, term_buffer::Terminal>,
    /// F-014: all app-side LSP orchestration (clients, diagnostics, hover/completion, request dispatch,
    /// deferred edit-applies) lives behind the coordinator; the loop just drives it (CAP-LSP-COORD).
    pub(crate) lsp: LspCoordinator,
    /// The status line's message and the `:messages` history (#1235).
    pub(crate) status: Status,
    /// How many `:normal` runs are in progress (nested ones count each).
    normal_depth: usize,
    /// The key being routed was typed, not replayed from a macro (nvim's `KeyTyped`, for 'foldopen').
    key_typed: bool,
    /// A `:g` is running its command over the marked lines (Vim's `global_busy`): a nested `:g` runs on
    /// the current line only, and a `:s` that matches nothing is no error (#1245).
    global_busy: bool,
    /// The error a `:silent!` hid from the status line (Vim's `v:errmsg` still records it, #609); the
    /// headless harness reads and clears it.
    pub(crate) silenced_error: Option<String>,
    /// The alternate file name a `:w {file}`, `:sav` or `:file {name}` left (#610).
    alt_file: Option<crate::app::ex_more::AltFile>,
    /// The working directory before the last `:cd` (`:cd -`, #610).
    prev_dir: Option<PathBuf>,
    /// The `:filter` pattern of the command being run (#609).
    ex_filter: Option<(String, bool)>,
    /// A `:startinsert`/`:stopinsert` waiting for its command line to end (#609): `None` inside is
    /// `:stopinsert`.
    pending_insert: Option<Option<(crate::input::StartKind, bool)>>,
}

impl Session {
    pub(crate) fn new(init: SessionInit) -> Session {
        let SessionInit {
            path,
            fmt,
            initial,
            status,
            profile,
            term,
            cwd,
            trusted,
            rescue,
        } = init;
        // F-007: the frontend now drives a Workspace (buffers + views + windows), not a single
        // EditorState. With one Window this is byte-identical to the pre-Workspace path; `:split`/
        // `:vsplit` open more Windows onto the same buffer with independent cursors and scroll.
        let mut ws = Workspace::new(initial.clone());
        // ruse's shipped default: search is case-insensitive with smartcase (the most widely recommended vimrc
        // pair). Vim/Neovim default both OFF for script backwards-compat — a constraint ruse has no legacy of —
        // so the interactive default is the better one. Set at the Workspace level so it reaches every buffer;
        // the engine/oracle default stays vanilla-off. Runtime-overridable via `:set noic`/`:set noscs`.
        ws.set_default_search_case(true, true);
        // The initial buffer owns the session file (`path`/`fmt`): name it, and remember its id so `:w` on any
        // OTHER buffer (`:enew` scratch) declines rather than clobbering the file (F-007 multi-buffer).
        let file_buf = ws.focused_buffer();
        // The per-buffer file registry (F-007): the primary buffer joins it when opened with a path; `:e`
        // adds more, `:enew` scratch buffers stay out (so `:w` on them declines). `:w` writes files[focused].
        // Syntax highlighting lives in the frontend (core stays dep-free): one highlighter PER file buffer,
        // keyed by DocumentId (F-007 multi-buffer).
        let mut files: Files = HashMap::new();
        let mut highlighters: Highlighters = HashMap::new();
        if let Some(p) = path.as_ref() {
            ws.set_focused_buffer_name(p.display().to_string());
            attach_file(&mut files, &mut highlighters, file_buf, p.clone(), fmt);
        }
        // Profile selection (F-012 / F-013): `emacs` = Emacs, `native` = Native, else Vim.
        let emacs_profile = profile == "emacs";
        let engine = match profile {
            "emacs" => InputEngine::emacs(),
            "native" => InputEngine::native(),
            _ => InputEngine::new(),
        };
        // Caret gravity follows the profile (D-050 / RFC-0015): the Emacs profile rests point BETWEEN chars
        // (line/buffer end = after the last char), so its edits are not Vim-clamped. Vim keeps `OnChar`, and
        // Native reuses the Vim text grammar (NAT-1) so it keeps `OnChar` too.
        if emacs_profile {
            ws.set_caret_gravity(CaretGravity::BetweenChar);
        }
        let start_options = ws.focused().view.editing_options();
        let start_registers = ws.registers().filled();
        let trace_watch = crate::app::trace_check::TraceWatch::new(&ws, &files);
        Session {
            ws,
            files,
            highlighters,
            line_idx: line_index::LineIndex::default(),
            pane_idx: line_index::PaneIndexes::default(),
            initial,
            recorded: Vec::new(),
            start_options,
            start_registers,
            trace_watch,
            journals: if rescue {
                persist::journal::current()
            } else {
                None
            },
            journal_writers: HashMap::new(),
            journal_failed: HashSet::new(),
            disk_changed: HashSet::new(),
            disk_warned: HashMap::new(),
            last_input: std::time::Instant::now(),
            disk_checked: std::time::Instant::now(),
            readonly: crate::app::dispatch::ReadOnly::default(),
            unnamed_start: path.is_none().then_some(file_buf),
            rescue,
            profile_name: profile,
            emacs_profile,
            engine,
            quit: false,
            pager: crate::ui::message::Pager::default(),
            term,
            prefix: Prefix::None,
            folds: HashMap::new(),
            fold_anchors: HashMap::new(),
            fold_method: HashMap::new(),
            foldlevel: HashMap::new(),
            foldenable: HashMap::new(),
            fold_rev: HashMap::new(),
            meta_tail: None,
            macros: crate::keys::MacroState::new(),
            macro_undo: None,
            confirm: None,
            swap: None,
            swap_answer: None,
            search_hl: None,
            hlsearch_on: true,
            incsearch_on: true,
            fixeol_on: false,
            scrolloff: SCROLLOFF,
            ui_opts: crate::app::set_exec::UiOptions::default(),
            screen_want: None,
            scroll_valid: None,
            config_lmaps: Vec::new(),
            opt_sources: HashMap::new(),
            ex_verbose: false,
            config: crate::config::Config::default(),
            config_paths: None,
            config_stamp: crate::config::Stamp::default(),
            boot_record: None,
            last_substitute: None,
            last_ex_line: None,
            can_cindent: true,
            last_shell: None,
            overlay: None,
            #[cfg(unix)]
            terminals: HashMap::new(),
            lsp: LspCoordinator::new(cwd, trusted),
            // The startup line is shown, not kept (nvim keeps no banner).
            status: Status::showing(Msg::echo(status)),
            normal_depth: 0,
            key_typed: false,
            global_busy: false,
            silenced_error: None,
            alt_file: None,
            prev_dir: None,
            ex_filter: None,
            pending_insert: None,
        }
    }

    /// Put the start buffer's recovered text over the disk text it opened with (#863, #1194): as recorded
    /// commands that undo as one step ([`crate::app::dispatch::apply_recovery`]), so the buffer is modified
    /// (`:q` refuses with E37), `u` goes back to the disk text, and a saved trace starts from the disk text
    /// and replays the recovery.
    pub(crate) fn apply_recovered(&mut self, rec: Vec<u8>) {
        crate::app::dispatch::apply_recovery(&mut self.ws, rec, &mut self.recorded);
        crate::app::dispatch::place_cursor(&mut self.ws, &mut self.recorded, 0);
    }

    /// Remove the journal of every buffer this session could have journaled (#862), on a normal quit: each
    /// writer's (also one beside a live other session's, #1193), each file buffer's, and the unnamed start
    /// buffer's. Only journals this process owns go ([`persist::journal::Journals::discard`]).
    pub(crate) fn discard_journals(&mut self) {
        for w in self.journal_writers.values_mut() {
            w.discard();
        }
        let Some(j) = self.journals else {
            return;
        };
        for bf in self.files.values() {
            j.discard(Some(bf.path.as_path()));
        }
        if self.unnamed_start.is_some() {
            j.discard(None);
        }
    }

    /// Sync every buffer's pending journal records now: before a risky operation (`:!`, a filter, `:r !`,
    /// `:w !`, `:terminal`, an LSP edit batch), so a hang or a kill inside it loses nothing that was typed
    /// (RFC-0023 §4).
    pub(crate) fn sync_journals(&mut self) {
        let Session {
            ws,
            files,
            journal_writers,
            journal_failed,
            status,
            ..
        } = self;
        sync_journals(ws, files, journal_writers, journal_failed, status);
    }

    /// How long until a buffered journal sync is due, for the event loop's poll timeout.
    fn journal_wake_ms(&self) -> Option<u64> {
        let now = std::time::Instant::now();
        self.journal_writers
            .values()
            .filter_map(|w| w.wake_in(now))
            .min()
            .map(|d| (d.as_millis() as u64).max(1))
    }

    /// Panic unless the focused buffer's folds are well formed over it (in range; a manual set flat, an
    /// indent set nested) and every manual fold anchor is live in its document — the fold part of the
    /// invariant walker (#906). Only the focused buffer's folds are reconciled each frame, so only they are
    /// checked against their text.
    pub(crate) fn assert_fold_invariants(&self) {
        let buf = self.ws.focused_buffer();
        if let Some(fv) = self.folds.get(&buf) {
            let lines = crate::app::dispatch::line_count(self.ws.focused().doc.bytes());
            let manual = self.fold_method.get(&buf) != Some(&crate::input::FoldMethod::Indent);
            crate::folds::assert_invariants(fv, lines, manual);
        }
        for (id, a) in &self.fold_anchors {
            if let Some(doc) = self.ws.document(*id) {
                for anchor in a.anchor_ids() {
                    assert!(
                        doc.resolve_anchor(anchor).is_some(),
                        "a fold anchor of buffer {id:?} is not live"
                    );
                }
            }
        }
    }

    /// The per-frame work that precedes the render and the key read: snapshot the focused buffer, refresh
    /// the line index, reconcile folds, publish the crash-rescue state, follow a `:s///c` match, and scroll
    /// each pane to its cursor. `cols`×`term_rows` is the terminal size.
    pub(crate) fn pre_frame(&mut self, cols: u16, term_rows: u16) -> FrameCtx {
        let global_so = self.scrolloff;
        let Session {
            ws,
            files,
            line_idx,
            pane_idx,
            journals,
            journal_writers,
            journal_failed,
            readonly,
            unnamed_start,
            rescue,
            folds,
            fold_anchors,
            fold_method,
            foldlevel,
            foldenable,
            fold_rev,
            confirm,
            status,
            lsp,
            ui_opts,
            scroll_valid,
            ..
        } = self;
        // Snapshot the FOCUSED buffer once for the frame (line index, folds, highlight parse). `text_arc()`
        // is an Arc refcount bump (not an O(buffer) copy): a committed edit builds a fresh Arc, so this handle
        // keeps pointing at the bytes captured here even after the later `&mut ws` borrows edit the doc.
        let (revision, snapshot) = {
            let f = ws.focused();
            (f.doc.revision(), f.doc.text_arc())
        };
        // Refresh the line index (rebuilds only on a revision change) so the per-frame row/viewport
        // lookups below are O(log n), not an O(buffer) newline scan. MVP splits share this one buffer.
        line_idx.refresh_for(ws.focused_buffer(), revision, &snapshot);
        // Fold reconcile (per frame): (a) carry manual folds through the edits since the last frame by their
        // line anchors (#819); (b) in Insert and Replace mode open the folds holding the cursor line, as nvim
        // does when Insert starts ("the cursor line is never folded"). The cursor may rest inside a closed
        // fold otherwise; it paints on the fold's summary row.
        {
            let buf = ws.focused_buffer();
            let indent = fold_method.get(&buf).copied() == Some(crate::input::FoldMethod::Indent);
            if indent {
                // Indent folds are DERIVED: recompute from indentation whenever the buffer content changed
                // (revision bump), then apply the foldlevel/foldenable rule. This resets any per-fold
                // za/zo/zc toggle on edit — an honest first slice; foldlevel drives closed state between
                // edits (zr/zm/zR/zM and :set foldlevel re-apply immediately, below).
                if fold_rev.get(&buf) != Some(&revision) {
                    let view = &ws.focused().view;
                    let mut computed = crate::folds::compute_indent_folds(
                        &snapshot,
                        view.tabs(),
                        view.shift_width(),
                    );
                    let en = foldenable.get(&buf).copied().unwrap_or(true);
                    let fdl = if en {
                        foldlevel.get(&buf).copied().unwrap_or(0)
                    } else {
                        usize::MAX // foldenable off ⇒ nothing closed
                    };
                    crate::folds::apply_foldlevel(&mut computed, fdl);
                    folds.insert(buf, computed);
                    fold_rev.insert(buf, revision);
                }
                // Derived folds need no anchors; manual ones made before `:set fdm=indent` lose theirs.
                if let Some(mut a) = fold_anchors.remove(&buf) {
                    a.release(ws.focused_doc_mut());
                }
            } else if let Some(fv) = folds.get_mut(&buf) {
                // Manual folds are NOT recomputed: their anchors carry them through the edits.
                let a = fold_anchors.entry(buf).or_default();
                a.follow(ws.focused().doc, fv, |o| line_idx.line_of(o));
                if !a.current(revision, fv) {
                    a.anchor(ws.focused_doc_mut(), fv, |l| line_idx.nth_line_start(l));
                }
            }
            if matches!(
                ws.focused().view.mode(),
                Mode::Insert | Mode::Replace | Mode::VirtualReplace
            ) {
                if let Some(fv) = folds.get_mut(&buf) {
                    crate::folds::open_at(fv, line_idx.line_of(ws.focused().view.cursor().0));
                }
            }
        }
        // Crash-rescue store + recovery journal cover EVERY recoverable buffer, not just the focused one
        // (#856): each file-backed buffer (keyed by its own path, F-007; incl. `:e`-opened ones) and the
        // pathless start buffer. A scratch buffer (`:enew`) is skipped so its bytes never land in some
        // file's recovery. `save()` clears a buffer's journal by path, so this stays consistent.
        readonly.follow(files);
        if *rescue {
            let now = std::time::Instant::now();
            let mut entries = Vec::with_capacity(files.len() + 1);
            for (id, bf) in files.iter() {
                let Some(doc) = ws.document_mut(*id) else {
                    continue;
                };
                let Some(j) = *journals else {
                    // No state directory: journaling is off, and the first modified buffer says so once.
                    if doc.is_modified() && journal_failed.insert(*id) {
                        status.error(format!(
                            "E303: unable to write the recovery journal for \"{}\": no state directory \
                             (set XDG_STATE_HOME or HOME)",
                            bf.path.display()
                        ));
                    }
                    continue;
                };
                let w = journal_writers
                    .entry(*id)
                    .or_insert_with(|| persist::journal::Writer::new(j, &bf.path, bf.fmt));
                w.follow(j, &bf.path);
                // Drain the change log (and drop the journal of a buffer that became clean: undone back to
                // the saved text, saved, or re-read with `:e!`, #862), then sync if the policy says so.
                w.absorb(doc, bf.fmt, now);
                if w.due(now) {
                    let r = w.flush(doc);
                    journal_outcome(r, *id, &bf.path, w.journal(), journal_failed, status);
                }
                entries.push(recover::Entry {
                    path: Some(bf.path.clone()),
                    journal: w.journal().to_path_buf(),
                    canonical: Some(w.doc().to_path_buf()),
                    format: bf.fmt,
                    bytes: doc.text_arc(),
                    modified: doc.is_modified(),
                    revision: doc.revision().0,
                    node: doc.history().current_seq().0,
                });
            }
            if let (Some(doc), Some(j)) = (unnamed_start.and_then(|id| ws.document(id)), *journals)
            {
                entries.push(recover::Entry {
                    path: None,
                    journal: j.unnamed(),
                    canonical: None,
                    format: persist::encoding::FileFormat::default(),
                    bytes: doc.text_arc(),
                    modified: doc.is_modified(),
                    revision: doc.revision().0,
                    node: doc.history().current_seq().0,
                });
            }
            // A buffer that is gone or lost its file (`:bd!`) lets its changes go with its journal.
            journal_writers.retain(|id, w| {
                let keep = files.contains_key(id) && ws.doc_is_live(*id);
                if !keep {
                    w.discard();
                }
                keep
            });
            recover::sync(entries);
        }
        // During a `:s///c` confirm, follow the current match so the viewport scrolls it into view.
        if let Some(c) = &confirm {
            if let Some(s) = c.subs.get(c.idx) {
                ws.place_focused_cursor(s.start);
                status.echo(confirm_prompt(c));
            }
        }
        // Per-window viewport pass: scroll each pane so ITS cursor stays visible in ITS rectangle
        // (F-007 acceptance #1 — independent scroll), counting the screen rows its wrapped lines take
        // (nvim's update_topline, #615), and sideways when it does not wrap. Geometry is shared with
        // render below.
        let text_rows = term_rows.saturating_sub(1);
        let rects = window_rects(cols, text_rows, ws.window_count(), ws.split_dir());
        let has_signs = !lsp.diagnostics_for(ws.focused_buffer()).is_empty();
        let focus_buf = ws.focused_buffer();
        // The other buffers the windows show keep their own line index, rebuilt on their own edits only.
        for i in 0..ws.window_count() {
            let p = ws.pane(i);
            if p.view.doc() != focus_buf {
                pane_idx.refresh(p.view.doc(), p.doc.revision(), p.doc.bytes());
            }
        }
        pane_idx.retain(|d| {
            d != focus_buf && (0..ws.window_count()).any(|i| ws.pane(i).view.doc() == d)
        });
        let mut wins = Vec::with_capacity(rects.len());
        for (i, rect) in rects.iter().enumerate() {
            let pane_folds: &[crate::folds::Fold] = if i == ws.focus() {
                folds.get(&focus_buf).map_or(&[][..], Vec::as_slice)
            } else {
                &[]
            };
            let (win, top, leftcol, skipcol) = {
                let p = ws.pane(i);
                let idx = if p.view.doc() == focus_buf {
                    &*line_idx
                } else {
                    pane_idx
                        .get(p.view.doc(), p.doc.revision())
                        .unwrap_or(&*line_idx)
                };
                let text = PaneText {
                    bytes: p.doc.bytes(),
                    idx,
                };
                let height = rect.h as usize;
                let mut win = WinView::resolve(
                    p.view,
                    &ui_opts.display,
                    rect.w as usize,
                    height,
                    text.line_count(),
                    i == ws.focus() && has_signs,
                )
                .with_folds(pane_folds);
                let cursor = p.view.cursor().0.min(text.bytes.len());
                let cline = idx.line_of(cursor);
                let lines = PaneLines {
                    text,
                    win: &win,
                    folds: pane_folds,
                    virt: &[],
                    height,
                };
                let so = scrolloff_for(p.view, global_so);
                // The cursor's line laid out up to the cursor (a huge line costs no more), once: the
                // scroll rules below read the same column.
                let line_byte = cursor - text.start(cline);
                let (start, end, vcol) = crate::ui::window_view::cursor_place(
                    &win,
                    text,
                    cline,
                    line_byte,
                    win.tab_end(p.view.mode()),
                );
                let sc = crate::scroll::Scroll {
                    lines: &lines,
                    height,
                    so,
                    sms: win.smoothscroll,
                    tab_end: win.tab_end(p.view.mode()),
                    memo: std::cell::Cell::new(Some((cline, line_byte, vcol))),
                };
                // The top line's skipped columns (#615): the cursor moved into them shows them again,
                // and a new top line starts unskipped.
                let mut pos = crate::scroll::ViewPos {
                    top: p.view.top(),
                    skipcol: if win.wrap { p.view.skipcol() } else { 0 },
                    line: cline,
                    byte: cursor - text.start(cline),
                    curswant: 0,
                };
                pos.skipcol = sc.uncover_cursor(&pos);
                if !win.smoothscroll && pos.skipcol > 0 {
                    // Without 'smoothscroll' a scroll's leftover skipped columns are gone once the
                    // cursor has moved (nvim's `curs_columns` runs before the top line is checked).
                    pos.skipcol = sc.frame_skipcol(&pos);
                }
                let skipped = crate::scroll::TopSkipped {
                    lines: &lines,
                    top: pos.top,
                    rows: sc.skipped_rows(pos.skipcol),
                };
                let kept = *scroll_valid
                    == Some(ScrollValid {
                        window: i,
                        cursor,
                        top: pos.top,
                        revision: p.doc.revision(),
                    });
                let sj = viewport::scrolljump_rows(ui_opts.display.scrolljump, height);
                let sms = (win.smoothscroll && win.wrap).then_some(win.width);
                let (top, sms_skip) = if kept {
                    (pos.top, None)
                } else {
                    let (top, skip) =
                        viewport::update_topline_sms(&skipped, pos.top, cline, height, so, sj, sms);
                    if top == pos.top || skip.is_some_and(|s| s > 0) {
                        (top, skip)
                    } else {
                        // nvim validates the top line again before it draws: a far jump centred
                        // without 'scrolloff' (`scroll_cursor_halfway`) then gets its context rows.
                        (
                            viewport::update_topline(&lines, top, cline, height, so, sj),
                            skip,
                        )
                    }
                };
                if top != pos.top || sms_skip.is_some() {
                    pos.top = top;
                    pos.skipcol = sms_skip.unwrap_or(0);
                }
                // A cursor line taller than the window shows the rows around the cursor (nvim's
                // `curs_columns`); 'smoothscroll' keeps what the scroll commands left.
                pos.skipcol = sc.frame_skipcol(&pos);
                let skip = sc.skipped_rows(pos.skipcol);
                let skipcol = pos.skipcol;
                let leftcol = if win.wrap {
                    0
                } else {
                    crate::ui::window_view::follow_leftcol(&win, start, end, vcol)
                };
                win.skip = skip;
                win.leftcol = leftcol;
                win.cursor = Some((cursor, vcol));
                (win, top, leftcol, skipcol)
            };
            ws.set_top(i, top);
            ws.set_leftcol(i, leftcol);
            ws.set_skipcol(i, skipcol);
            // The window's text columns: 'wrapmargin' counts from its right edge, after the gutter.
            ws.set_text_cols(i, win.width);
            wins.push(win);
        }
        FrameCtx {
            revision,
            snapshot,
            rects,
            wins,
            cols,
            text_rows,
        }
    }

    /// The per-frame work after the render: apply the LSP coordinator's deferred results (opening a buffer
    /// mutates `highlighters`, which the frame's highlight spans borrowed until render ended), then open the
    /// references / code-action pickers it collected.
    pub(crate) fn post_render(&mut self, frame: &FrameCtx) {
        // An LSP edit batch (format, rename, a server's `workspace/applyEdit`) is a risky operation: the
        // typing before it reaches the journal first (RFC-0023 §4, #1202).
        if self.lsp.has_edit_batch() {
            self.sync_journals();
        }
        let recorded = &mut self.recorded;
        self.lsp.apply_pending(
            &mut self.ws,
            &mut self.files,
            &mut self.highlighters,
            &frame.snapshot,
            &mut self.status,
            &mut |ws, ex| run_ex(ex, ws, recorded),
        );
        if let Some(locs) = self.lsp.take_refs() {
            self.overlay = Some(Overlay::Refs(ref_picker::open(locs, self.lsp.cwd())));
        }
        if let Some(actions) = self.lsp.take_actions() {
            self.overlay = Some(Overlay::Actions(action_picker::open(actions)));
        }
    }

    /// Async output (PTY, F-011; LSP diagnostics, F-014) must render without a keypress, so the loop polls
    /// with a timeout while either is live. With neither it keeps the pure blocking read (no spin) — a
    /// terminal ticks fast, an LSP-only session more slowly.
    pub(crate) fn poll_ms(&self) -> Option<u64> {
        let mut poll_ms: Option<u64> = None;
        #[cfg(unix)]
        if !self.terminals.is_empty() {
            poll_ms = Some(TERM_TICK_MS);
        }
        if poll_ms.is_none() && self.lsp.has_live_client() {
            poll_ms = Some(LSP_TICK_MS);
        }
        // Wake for a journal sync that falls due while no key arrives (RFC-0023 §4's idle trigger), and
        // for the idle look at the files on disk (§8).
        for ms in [self.journal_wake_ms(), self.disk_wake_ms()]
            .into_iter()
            .flatten()
        {
            poll_ms = Some(poll_ms.map_or(ms, |p| p.min(ms)));
        }
        poll_ms
    }

    /// The status-line flags after the focused buffer's `[+]` (RFC-0023 §8): `[RO]` for a `'readonly'` buffer (nvim's `%r`, #1211), `[journal!]` when the last
    /// write or sync of its modified buffer's journal failed (#938).
    pub(crate) fn status_flags(&self) -> String {
        let mut flags = String::new();
        let id = self.ws.focused_buffer();
        if self.readonly.get(&self.files, id) {
            flags.push_str("[RO]");
        }
        if self.journal_failed.contains(&id) && self.ws.focused().doc.is_modified() {
            flags.push_str("[journal!]");
        }
        if self.disk_changed.contains(&id) {
            flags.push_str("[disk changed]");
        }
        if self.config.safe_mode.is_some() {
            flags.push_str("[Config: safe-mode]");
        }
        flags
    }

    /// Look at every file buffer's file again (RFC-0023 §8): a stat, and a hash when it moved. A buffer
    /// whose file newly differs from what ruse read or saved gets nvim's W11 (unmodified) or W12 (modified
    /// too) on the status line when `announce`, the focused buffer first; one whose file is gone gets
    /// nvim's E211 and no `[disk changed]` flag. ruse does not offer to reload.
    pub(crate) fn check_disk(&mut self, announce: bool) {
        self.disk_checked = std::time::Instant::now();
        // The config layer files too (#1152); a file's own warning, below, is shown over its report.
        self.watch_config();
        let focused = self.ws.focused_buffer();
        let mut ids: Vec<DocumentId> = self.files.keys().copied().collect();
        ids.sort_by_key(|id| (*id != focused, id.0));
        let mut first = None;
        for id in ids {
            let Some(bf) = self.files.get(&id) else {
                continue;
            };
            let state = persist::disk::check(&bf.path);
            if state == persist::disk::State::Changed {
                self.disk_changed.insert(id);
            } else {
                self.disk_changed.remove(&id);
            }
            if state == persist::disk::State::Same {
                self.disk_warned.remove(&id);
            } else if self.disk_warned.get(&id) != Some(&state) && first.is_none() {
                first = Some((id, state));
            }
        }
        self.disk_changed.retain(|id| self.files.contains_key(id));
        self.disk_warned.retain(|id, _| self.files.contains_key(id));
        let Some((id, state)) = first.filter(|_| announce) else {
            return;
        };
        self.disk_warned.insert(id, state);
        let (Some(bf), Some(doc)) = (self.files.get(&id), self.ws.document(id)) else {
            return;
        };
        let name = bf.path.display();
        if state == persist::disk::State::Gone {
            self.status
                .error(format!("E211: File \"{name}\" no longer available"));
        } else if doc.is_modified() {
            self.status.warn(format!(
                "W12: Warning: File \"{name}\" has changed and the buffer was changed in Vim as well"
            ));
        } else {
            self.status.warn(format!(
                "W11: Warning: File \"{name}\" has changed since editing started"
            ));
        }
    }

    /// The idle trigger of [`Session::check_disk`]: once [`persist::journal::IDLE`] passed since the last
    /// key and the last check. Only a session with files on disk to watch (the binary's) runs it.
    pub(crate) fn idle_disk_check(&mut self) {
        let idle = persist::journal::IDLE;
        if self.rescue
            && (!self.files.is_empty() || self.config_paths.is_some())
            && self.last_input.elapsed() >= idle
            && self.disk_checked.elapsed() >= idle
        {
            self.check_disk(true);
        }
    }

    /// How long until [`Session::idle_disk_check`] is due, for the poll timeout.
    fn disk_wake_ms(&self) -> Option<u64> {
        if !self.rescue || (self.files.is_empty() && self.config_paths.is_none()) {
            return None;
        }
        let idle = persist::journal::IDLE;
        let wait = idle
            .saturating_sub(self.last_input.elapsed())
            .max(idle.saturating_sub(self.disk_checked.elapsed()));
        Some((wait.as_millis() as u64).max(1))
    }

    /// Whether keys of a macro replay are still to run (the event loop paints less often meanwhile).
    pub(crate) fn replaying(&self) -> bool {
        self.macros.replay_pending()
    }

    /// The next key the session queued for itself, if any, with its `from_replay` flag: the key half of a
    /// split Alt chord (typed, #896), else the next key of a macro replay (`@{reg}`, `@:`). The caller runs
    /// a full frame before each one and feeds it to [`Session::on_key`] before reading the terminal.
    pub(crate) fn next_queued(&mut self) -> Option<(KeyEvent, bool)> {
        if let Some(k) = self.meta_tail.take() {
            return Some((k, false));
        }
        // CTRL-C between two replayed keys stops the replay (a runaway macro, #787 / #898).
        if self.macros.replay_pending() && ruse_core::interrupt::breakcheck() {
            self.settle_interrupt();
            return None;
        }
        // A linewise-Visual `@{reg}` runs on each line from its column 0, clamped to the last line.
        if let Some(line) = self.macros.next_line_run() {
            let b = self.ws.focused().doc.bytes();
            let lines = ruse_core::pos::count_newlines(b) + usize::from(b.last() != Some(&b'\n'));
            let at = ruse_core::pos::nth_line_start(b, line.min(lines.saturating_sub(1))).0;
            place_cursor(&mut self.ws, &mut self.recorded, at);
        }
        let next = self.macros.next_replay();
        if next.is_none() {
            self.end_macro_undo();
        }
        next.map(|k| (k, true))
    }

    /// Close the undo block a macro replay opened ([`Session::macro_undo`]).
    fn end_macro_undo(&mut self) {
        if let Some(doc) = self.macro_undo.take() {
            if let Some(d) = self.ws.document_mut(doc) {
                d.end_undo_block();
            }
            note_ex(ExCmd::End, &mut self.recorded);
        }
    }

    /// Vim's undo sync before a TYPED key (#678): a key read outside text entry, or by an `i_CTRL-O`
    /// one-shot, begins a new command, so its change is an undo step of its own; Insert keeps typing in one
    /// step, except the text typed after an `i_CTRL-O` command returns (a new insert). Keys that were not
    /// typed (a macro replay, `:normal`) never sync. The Emacs profile keeps its grouping (consecutive edits
    /// until a motion).
    fn sync_undo_before_typed(&mut self) {
        if self.emacs_profile {
            return;
        }
        let mode = self.ws.focused().view.mode();
        let entry = matches!(mode, Mode::Insert | Mode::Replace | Mode::VirtualReplace);
        if !entry || self.engine.in_one_shot() || self.engine.insert_resumed() {
            // Recorded when it closes a group, so a replay's `u` undoes what it undid live (RFC-0020).
            if !self.ws.focused().doc.history().is_synced() {
                note_ex(ExCmd::UndoSync, &mut self.recorded);
            }
            self.ws.focused_doc_mut().sync_undo();
        }
    }

    /// The key typed at the prompt the last frame showed (true: the key is used up). At the hit-enter
    /// prompt (#1222) it clears the message: `<CR>`, `<NL>`, `<Space>` and `CTRL-C` only do that; any other
    /// key, `<Esc>` too, then runs as typed, `:` opening the command line, as nvim v0.12.6's `wait_return`
    /// puts it back into the typeahead. At `-- More --` (#1235) the paging keys move the message and `q`
    /// ends it ([`crate::ui::message::Pager::more_key`]).
    fn answer_prompt(&mut self, key: KeyEvent) -> bool {
        use crate::ui::message::{Answer, Prompt};
        let answer = match std::mem::take(&mut self.pager.prompt) {
            Prompt::None => return false,
            Prompt::HitEnter => self.pager.hit_enter_key(key),
            Prompt::More => self.pager.more_key(key),
        };
        match answer {
            Answer::Stay => true,
            Answer::Done => {
                self.status.clear();
                true
            }
            Answer::DoneThenRun => {
                self.status.clear();
                false
            }
        }
    }

    /// Route one key through the session (#878): the macro state machine, the LSP hover / completion
    /// intercepts, a `:s///c` confirm loop, an open picker, a terminal buffer, the session prefixes and
    /// chords, `H`/`M`/`L`, and finally the input engine and its resolution. `frame` is the frame built
    /// for this key by [`Session::pre_frame`].
    pub(crate) fn on_key(
        &mut self,
        raw_key: KeyEvent,
        from_replay: bool,
        frame: &FrameCtx,
    ) -> Flow {
        let status_before = self.status.serial();
        self.last_input = std::time::Instant::now();
        if !from_replay {
            // A typed key comes after any replay drained; a block left open would join this key's change.
            self.end_macro_undo();
            self.sync_undo_before_typed();
        }
        let ro_watch = self.readonly.watch(&self.ws, &self.files);
        let trace_before = (self.recorded.len(), self.ws.focused_buffer());
        // A replayed key's edits are the macro's (INV-ORIGIN, #880).
        self.key_typed = !from_replay;
        self.route_as(raw_key, from_replay, frame);
        self.key_typed = false;
        self.readonly.warn(ro_watch, &self.ws, &mut self.status);
        self.trace_watch
            .after_input(trace_before, &mut self.ws, &mut self.recorded);
        if !from_replay && !self.emacs_profile {
            // A typed `@{reg}` (or `@:`, `Q`) queued its replay: one undo step for all of it (#939).
            if self.macros.replay_pending() && !self.ws.focused().doc.in_undo_block() {
                self.ws.focused_doc_mut().begin_undo_block();
                self.macro_undo = Some(self.ws.focused_buffer());
                note_ex(ExCmd::Begin(GroupKind::Macro), &mut self.recorded);
            }
        }
        if let Some(key) = self.swap_answer.take() {
            if let Some(prompt) = self.swap.take() {
                self.answer_swap(prompt, key, frame);
            }
        }
        if self.failed_since(status_before) {
            self.macros.flush_replay();
        }
        self.settle_interrupt();
        if self.quit {
            Flow::Quit
        } else {
            Flow::Continue
        }
    }

    /// After a key: if CTRL-C interrupted what it ran ([`ruse_core::interrupt`]), every loop on the way out
    /// has stopped; drop the keys still to replay (nvim flushes typeahead on `got_int`), close the replay's
    /// undo step, say so, and clear the interrupt for the next command.
    fn settle_interrupt(&mut self) {
        if ruse_core::interrupt::take() {
            self.macros.interrupt_replay();
            self.end_macro_undo();
            self.status.info("Interrupted");
        }
    }

    /// Route one bracketed paste (#600, `:help paste`). The text never becomes keys, so no mapping, macro
    /// prefix, session prefix or chord sees it. Line endings are normalised first (CRLF and CR become LF,
    /// as nvim's TUI pastes). A terminal buffer forwards it to its child (bracketed if the child asked);
    /// a picker or a `:s///c` confirm loop declines it with a message; everything else goes through
    /// [`InputEngine::paste`]. A paste does not end a macro recording and is not recorded into it.
    pub(crate) fn on_paste(&mut self, raw: &str, frame: &FrameCtx) -> Flow {
        self.last_input = std::time::Instant::now();
        let text = crate::input::paste_text(raw);
        if text.is_empty() {
            return Flow::Continue;
        }
        self.lsp.clear_hover();
        self.prefix = Prefix::None;
        self.meta_tail = None;
        if self.overlay_open() {
            self.status.warn("paste ignored: close the picker first");
            return Flow::Continue;
        }
        let mode = self.ws.focused().view.mode();
        #[cfg(unix)]
        if matches!(mode, Mode::Terminal | Mode::TerminalNormal) {
            if let Some(t) = self.terminals.get_mut(&self.ws.focused_buffer()) {
                t.paste(&text);
                return Flow::Continue;
            }
        }
        self.sync_undo_before_typed();
        let visual_append = self.visual_paste_appends(mode, &frame.snapshot);
        let out = self.engine.paste(&text, mode, visual_append);
        let ro_watch = self.readonly.watch(&self.ws, &self.files);
        let report = self.ui_opts.report;
        let trace_before = (self.recorded.len(), self.ws.focused_buffer());
        let Session {
            ws,
            files,
            recorded,
            status,
            quit,
            ..
        } = self;
        match out {
            Feed::Cmd(cmd) => run_cmd(cmd, ws, files, recorded, status, quit),
            Feed::Replay(cmds) => {
                for cmd in cmds {
                    run_cmd(cmd, ws, files, recorded, status, quit);
                }
            }
            Feed::Repeat(r) => run_repeat(r, ws, files, recorded, status, quit, report),
            _ => {}
        }
        self.readonly.warn(ro_watch, &self.ws, &mut self.status);
        self.trace_watch
            .after_input(trace_before, &mut self.ws, &mut self.recorded);
        self.settle_interrupt();
        if self.quit {
            Flow::Quit
        } else {
            Flow::Continue
        }
    }

    /// For a paste over a blockwise Visual/Select selection: whether deleting the block leaves the caret
    /// past the end of its top row, so the text must be appended (`a`) rather than inserted (`i`) to land
    /// where the block began, as nvim does. Charwise and linewise pastes replace the selection in the core.
    fn visual_paste_appends(&self, mode: Mode, bytes: &[u8]) -> bool {
        let (Mode::Visual { kind } | Mode::Select { kind }) = mode else {
            return false;
        };
        if kind != ruse_core::SelectKind::Blockwise {
            return false;
        }
        let span = self
            .ws
            .focused()
            .view
            .block_spans(bytes)
            .and_then(|rows| rows.first().copied());
        let Some((BytePos(s), BytePos(e))) = span else {
            return false;
        };
        let at_line_start = s == 0 || bytes.get(s - 1) == Some(&b'\n');
        let rest_empty = matches!(bytes.get(e), None | Some(b'\n'));
        !at_line_start && rest_empty
    }

    /// Whether a modal overlay owns the keystream: a `:s///c` confirm loop or an open picker. The macro
    /// layer must not take its `q`/`@` (#851).
    fn overlay_open(&self) -> bool {
        self.confirm.is_some() || self.swap.is_some() || self.overlay.is_some()
    }

    /// The focused window's `'scrolloff'` as the rows of context the viewport keeps.
    fn scrolloff_rows(&self) -> usize {
        scrolloff_for(self.ws.focused().view, self.scrolloff)
    }

    /// Install the loaded config ([`Session::config`]) into the options, with provenance (#614). At
    /// `startup` nothing goes into the trace and the trace's starting options are read again afterwards
    /// (a trace starts from the configured values, RFC-0020 R4); a reload mid-session records its sets.
    pub(crate) fn apply_config(&mut self, startup: bool) {
        self.apply_config_keys(startup, None);
    }

    /// [`Session::apply_config`] for the keys in `only` alone (every key with `None`): a reload installs
    /// only what changed, so an unchanged key leaves a window's local value alone.
    fn apply_config_keys(&mut self, startup: bool, only: Option<&[&'static str]>) {
        let mut scratch = Vec::new();
        let Session {
            ws,
            recorded,
            engine,
            hlsearch_on,
            search_hl,
            incsearch_on,
            fixeol_on,
            scrolloff,
            folds,
            fold_method,
            foldlevel,
            foldenable,
            fold_rev,
            opt_sources,
            ui_opts,
            config,
            files,
            readonly,
            ..
        } = self;
        let mut host = crate::app::set_exec::OptionHost {
            ws,
            recorded: if startup { &mut scratch } else { recorded },
            engine,
            hlsearch_on,
            search_hl,
            incsearch_on,
            fixeol_on,
            scrolloff,
            folds,
            fold_method,
            foldlevel,
            foldenable,
            fold_rev,
            sources: opt_sources,
            ui: ui_opts,
            files,
            readonly,
            win_height: 0,
        };
        crate::app::set_exec::apply_config_keys(config, &mut host, only);
        let notes = if only.is_none_or(|k| k.contains(&"keymap.lang")) {
            crate::app::set_exec::install_lang_keymap(config, engine, &mut self.config_lmaps)
        } else {
            Vec::new()
        };
        if !notes.is_empty() {
            let shown = format!("{} · {}", self.status, notes.join(" · "));
            self.status.amend(shown);
        }
        if startup {
            self.start_options = self.ws.focused().view.editing_options();
            self.start_registers = self.ws.registers().filled();
        }
    }

    /// `:trust` (`grant`) / `:untrust`: write or delete the user-side trust record of the workspace root
    /// (its `.ruse/` content hash, D-062), then reload the config so the workspace layer comes in or goes.
    /// `:untrust` also takes back the session's language-server grant (servers already running keep
    /// running until they exit).
    fn change_config_trust(&mut self, grant: bool) {
        if !grant {
            self.lsp.revoke_trust();
        }
        let paths = self.config_paths.clone();
        let target = paths.as_ref().and_then(|p| {
            Some((
                p.workspace_root.clone()?,
                crate::trust::TrustStore::new(p.trust_dir.clone()?),
            ))
        });
        let Some((root, store)) = target else {
            if !grant {
                self.status.info("workspace untrusted for this session");
            }
            return;
        };
        let note = if grant {
            match crate::trust::ruse_dir_hash(&root).and_then(|h| store.record(&root, &h)) {
                Ok(_) => None,
                Err(e) => Some(format!("trust record not written: {e}")),
            }
        } else {
            Some(match store.remove(&root) {
                Ok(true) => "workspace untrusted: its config is no longer loaded".to_string(),
                Ok(false) => "workspace untrusted for this session (it had no trust record)".into(),
                Err(e) => format!("trust record not removed: {e}"),
            })
        };
        if let Some(paths) = paths {
            self.config = crate::config::load_mode(&paths, self.config.safe_mode);
            self.config_stamp = crate::config::Stamp::of(&paths);
            self.apply_config(false);
        }
        let workspace_loaded = self.config.layers.iter().any(|l| {
            l.kind == crate::config::LayerKind::Workspace
                && l.state == crate::config::LayerState::Loaded
        });
        match note {
            Some(n) if grant => {
                let shown = format!("{}; {n}", self.status);
                self.status.amend(shown);
            }
            Some(n) => self.status.info(n),
            None if workspace_loaded => {
                let shown = format!("{}; workspace config loaded", self.status);
                self.status.amend(shown);
            }
            None => {}
        }
    }

    /// Reload the config when a layer file changed on disk since it was loaded (#1152). Called where the
    /// session looks at its files again: when idle, on focus gain, after an Ex command.
    pub(crate) fn watch_config(&mut self) {
        let Some(paths) = &self.config_paths else {
            return;
        };
        if crate::config::Stamp::of(paths) != self.config_stamp {
            self.reload_config(false);
        }
    }

    /// Load the config layers again and install what changed (`config-model.md` Performance: the merge
    /// is recomputed and diffed). `explicit` is `:config-reload`, which also leaves safe mode (and clears
    /// the crash-loop record) and always reports; an automatic reload keeps safe mode and is quiet when no
    /// value changed. A layer that loaded before and now does not parse keeps the whole previous config,
    /// and the error is reported. A key that only applies at startup (`input.profile`, security-locked
    /// keys) keeps its running value and is named as taking effect at the next start.
    pub(crate) fn reload_config(&mut self, explicit: bool) {
        use crate::config::LayerState;
        let Some(paths) = self.config_paths.clone() else {
            if explicit {
                self.status
                    .error("config-reload: no config files in this session");
            }
            return;
        };
        let safe = if explicit {
            None
        } else {
            self.config.safe_mode
        };
        let mut new = crate::config::load_mode(&paths, safe);
        self.config_stamp = crate::config::Stamp::of(&paths);
        for d in &new.diagnostics {
            crate::log::config_diagnostic(d.code);
        }
        let broken = new.layers.iter().find(|l| {
            matches!(l.state, LayerState::Quarantined(_))
                && self
                    .config
                    .layers
                    .iter()
                    .any(|o| o.kind == l.kind && o.state == LayerState::Loaded)
        });
        if let Some(l) = broken {
            let why = match &l.state {
                LayerState::Quarantined(why) => why.clone(),
                _ => String::new(),
            };
            let text = format!(
                "config not reloaded, the previous one stays: {}: {why}",
                crate::config::short_path(&l.path)
            );
            if explicit {
                self.status.error(text);
            } else {
                self.status.warn(text);
            }
            return;
        }
        let changed = crate::config::changed_keys(&self.config, &new);
        let (live, later): (Vec<&'static str>, Vec<&'static str>) =
            changed.iter().partition(|k| crate::config::reloads_live(k));
        // A key that applies only at startup keeps its running value in the snapshot.
        for k in &later {
            match self.config.values.get(k) {
                Some(e) => new.values.insert(k, e.clone()),
                None => new.values.remove(k),
            };
        }
        let left_safe = self.config.safe_mode.is_some() && new.safe_mode.is_none();
        self.config = new;
        self.apply_config_keys(false, Some(&live));
        if left_safe {
            if let Some(b) = &self.boot_record {
                b.clear();
            }
        }
        if !explicit && changed.is_empty() {
            return;
        }
        let mut text = match live.len() {
            0 if left_safe => "config reloaded; safe mode off".to_string(),
            0 => "config reloaded: no change".to_string(),
            n => format!(
                "config reloaded{}: {n} key(s) changed ({})",
                if left_safe { "; safe mode off" } else { "" },
                live.join(", ")
            ),
        };
        if !later.is_empty() {
            text.push_str(&format!("; at the next start: {}", later.join(", ")));
        }
        if !self.config.diagnostics.is_empty() {
            text.push_str(&format!(
                " (+{} diagnostic(s); :inspect config)",
                self.config.diagnostics.len()
            ));
        }
        self.status.info(text);
    }

    fn route(&mut self, raw_key: KeyEvent, from_replay: bool, frame: &FrameCtx) {
        let so = self.scrolloff_rows();
        let report = self.ui_opts.report;
        let overlay_open = self.overlay_open();
        let Session {
            ws,
            files,
            highlighters,
            line_idx,
            recorded,
            emacs_profile,
            engine,
            quit,
            prefix,
            folds,
            fold_method,
            foldlevel,
            foldenable,
            macros,
            confirm,
            swap,
            swap_answer,
            search_hl,
            last_substitute,
            last_ex_line,
            overlay,
            #[cfg(unix)]
            terminals,
            lsp,
            status,
            profile_name,
            meta_tail,
            alt_file,
            screen_want,
            scroll_valid,
            journal_writers,
            journal_failed,
            ..
        } = self;
        let (revision, text_rows) = (frame.revision, frame.text_rows);
        let (snapshot, rects) = (&frame.snapshot, &frame.rects);
        if raw_key.kind == KeyEventKind::Release {
            return;
        }
        // An Alt/Meta key under the Vim profile is `<Esc>` then the key, as in nvim v0.12.6 for an unmapped
        // `<M-x>` (ruse has no Alt mappings in Vim). Typing `<Esc>x` fast, or over a slow link, delivers both
        // bytes in one read, which the terminal decoder reports as Alt-x (#896).
        let raw_key = if !from_replay
            && *profile_name == "vim"
            && raw_key.modifiers.contains(KeyModifiers::ALT)
        {
            let mut tail = raw_key;
            tail.modifiers -= KeyModifiers::ALT;
            *meta_tail = Some(tail);
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)
        } else {
            raw_key
        };
        // Run the key through the macro state machine (capture / stop / `q`|`@` prefixes). Only a
        // `Dispatch` falls through to the normal engine + intercepts; the rest are handled here. `q`, `@`
        // and `Q` are commands only at a command boundary (#679, #680, #851): not while an overlay or a
        // session prefix (`C-w`, `z`, …) owns the key, not in Insert or on the command line, and not while
        // the engine waits for one (`fq`, `"q`, `gq`, `dq`, a Native leader key), where the `q` or `@` is
        // that key. `q`, `@` and `Q` also work from Visual (nvim 0.12.6, #1068), but not from Select.
        let boundary =
            !overlay_open && matches!(prefix, Prefix::None) && engine.pending() == Pending::Base;
        let mode = ws.focused().view.mode();
        // Not under the modeless Emacs profile, whose `q` and `@` self-insert (#1156).
        let macro_cmd =
            boundary && !*emacs_profile && matches!(mode, Mode::Normal | Mode::Visual { .. });
        let step = macros.step_gated(raw_key, from_replay, macro_cmd, macro_cmd);
        // A count typed before `q` (record or stop), `Q` with nothing recorded, or an aborted register name
        // is discarded, as in nvim (`3qaxq` deletes one char); only an armed `@` keeps it for its replay.
        if matches!(
            step,
            crate::keys::Step::Consumed
                | crate::keys::Step::Store(..)
                | crate::keys::Step::OpenCmdWin(_)
        ) && !macros.awaits_replay_register()
        {
            engine.take_count();
        }
        let key = match step {
            crate::keys::Step::Dispatch(k) => k,
            crate::keys::Step::Consumed => return,
            crate::keys::Step::NoPreviousRegister => {
                engine.take_count();
                status.error("E748: No previously used register");
                return;
            }
            // `q:` / `q/` / `q?` — open the command-line window (`:help cmdwin`) over that history ring.
            crate::keys::Step::OpenCmdWin(kind) => {
                engine.open_cmdwin(kind);
                return;
            }
            crate::keys::Step::Store(reg, bytes) => {
                let n = bytes.len();
                // Recorded as the slots it changed (RFC-0020 R6): no recorded command wrote them.
                record_register_writes(ws, recorded, |ws| ws.set_register_raw(Some(reg), bytes));
                status.echo(format!("recorded @{reg} ({n} keys)"));
                return;
            }
            crate::keys::Step::Replay(reg) => {
                // `{count}@{reg}` repeats the macro `count` times; the count was accumulated by the engine
                // from the digits typed before `@`, so consume it here. The recursion budget is shared
                // across all copies, so `999@a` of a big macro still terminates.
                let n = engine.take_count().max(1);
                let bytes = ws.register_bytes(Some(reg));
                // In linewise Visual nvim runs the register on each selected line instead (`v_@-default`:
                // `:'<,'>normal! @{reg}`), ignoring the count: leave Visual, then one run per line.
                if mode
                    == (Mode::Visual {
                        kind: ruse_core::SelectKind::Linewise,
                    })
                {
                    let pane = ws.focused();
                    let b = pane.doc.bytes();
                    if let Some((BytePos(s), BytePos(e))) = pane.view.selection_span(b) {
                        let first = ruse_core::pos::line_of(b, ruse_core::BytePos(s));
                        let last = ruse_core::pos::line_of(
                            b,
                            ruse_core::BytePos(e.saturating_sub(1).max(s)),
                        );
                        macros.replay(b"\x1b");
                        macros.replay_lines(first, last, bytes);
                    }
                    return;
                }
                // An empty register replays nothing however large the count (#685).
                if crate::keys::decode(&bytes).is_empty() {
                    return;
                }
                for _ in 0..n {
                    if !macros.replay(&bytes) {
                        status.warn("macro replay aborted (key limit)");
                        break;
                    }
                }
                return;
            }
            // `[count]@:` — repeat the last-executed Ex command-line (`:help @:`). We re-enqueue the stored
            // `":`-register line as `:` + text + `<CR>` into the SAME macro-replay queue `@{reg}` uses, so
            // the keys re-drive the whole Ex pipeline (`engine.feed` → `Feed::ExecuteEx` below) exactly as
            // if re-typed — no second Ex executor. `[count]` enqueues that many copies. No prior Ex line is
            // Vim's E30 (a no-op edit-wise); verified vs nvim v0.12.4.
            crate::keys::Step::RepeatEx => {
                let n = engine.take_count().max(1);
                match &last_ex_line {
                    Some(line) => {
                        let mut bytes = Vec::with_capacity(line.len() + 2);
                        bytes.push(b':');
                        bytes.extend_from_slice(line.as_bytes());
                        bytes.push(b'\r');
                        for _ in 0..n {
                            if !macros.replay(&bytes) {
                                status.warn("macro replay aborted (key limit)");
                                break;
                            }
                        }
                    }
                    None => status.error("E30: No previous command line"),
                }
                return;
            }
        };
        // F-014: any key dismisses a shown hover panel (a fresh `K` re-populates it after its response).
        lsp.clear_hover();
        // F-014 #5: the completion pum owns the keystream while open (nav / accept / dismiss); `<C-x><C-o>`
        // in Vim/Native Insert opens it. Each returns whether it consumed the key (so the loop `continue`s).
        if lsp.on_completion_key(key, ws, status, &mut |ws, ex| run_ex(ex, ws, recorded)) {
            return;
        }
        if lsp.on_omni_key(key, ws, files, snapshot, revision, *emacs_profile, status) {
            return;
        }
        // A `:s///c` confirm loop owns the keystream while active: y/n/a/l/q per match (F-009 #2).
        if confirm.is_some() {
            confirm_key(confirm, key, ws, recorded, status, report);
            return;
        }
        // A backwards-range prompt owns the next key; [`Session::on_key`] answers it once routing is done.
        if swap.is_some() {
            *swap_answer = Some(key);
            return;
        }
        // A picker owns the keystream while open (F-004 / F-013 NAT-3): its transient keymap filters/moves,
        // Enter accepts (the session runs the payload-specific action below), Esc closes.
        if let Some(open) = overlay.as_mut() {
            let (outcome, accepted) = open.on_key(key);
            if !matches!(outcome, PickOutcome::Continue) {
                *overlay = None;
            }
            match accepted {
                None => {}
                // The line, diagnostics, marks and position pickers jump the cursor to a byte offset.
                Some(Accepted::Jump(offset)) => place_cursor(ws, recorded, offset),
                // The buffer picker switches the focused window to the selected buffer.
                Some(Accepted::Buffer(id)) => {
                    ws.focus_buffer(id);
                }
                // The file picker opens the selected path into a buffer (the interactive form of `:e`).
                Some(Accepted::File(p)) => {
                    status.give(
                        open_file_into_buffer(&p.display().to_string(), ws, files, highlighters)
                            .status,
                    );
                }
                // F-014: the references picker jumps to the selected location. The `spans` borrow is
                // released by now (render is done), so opening a cross-file buffer here is free — no defer
                // needed (unlike the response-loop goto). The open helper focuses the buffer already
                // holding this file (#848), so the current file and another open file both move without a
                // second Document.
                Some(Accepted::Location(uri, l, c)) => {
                    let path = crate::lsp::uri_to_path(&uri);
                    let target = std::fs::canonicalize(&path).unwrap_or(path);
                    let before = ws.focused_buffer();
                    let opened = open_file_into_buffer(
                        &target.display().to_string(),
                        ws,
                        files,
                        highlighters,
                    );
                    if opened.id.is_some() {
                        let bytes = ws.focused().doc.bytes();
                        let at = crate::lsp::model::lsp_pos_to_byte(bytes, l, c);
                        // A move in the same buffer is recorded (#1134); a trace does not follow a switch
                        // to another buffer, so a placement there is not.
                        if ws.focused_buffer() == before {
                            place_cursor(ws, recorded, at);
                        } else {
                            ws.place_focused_cursor(BytePos(at));
                        }
                    } else {
                        status.give(opened.status);
                    }
                }
                // F-014: the code-action picker applies the selected action's WorkspaceEdit (inline, as
                // the `spans` borrow is released post-render — like the references jump).
                Some(Accepted::Action(action)) => {
                    if !action.edit.is_empty() {
                        // An LSP edit batch syncs the journal first (RFC-0023 §4, #1202).
                        sync_journals(ws, files, journal_writers, journal_failed, status);
                    }
                    lsp.apply_code_action(
                        &action,
                        ws,
                        files,
                        highlighters,
                        status,
                        &mut |ws, ex| run_ex(ex, ws, recorded),
                    );
                }
                // The command palette dispatches the selected command by its stable id, through the same
                // path as a typed key's command (so it is resolved, undone and recorded like any other,
                // #689).
                Some(Accepted::Command(cmd)) => self.run_resolved_cmd(cmd, frame),
            }
            return;
        }
        // F-011 terminal routing: a terminal buffer owns the keystream. In Terminal mode keys forward to the
        // PTY child, except `CTRL-\ CTRL-N` which drops to Terminal-Normal. In Terminal-Normal, `i`/`a`/`A`
        // resume Terminal; every other key falls through to the normal grammar (so `:q`, `C-w`, etc. work) —
        // the placeholder document is empty, so slice 1 has no scrollback paging (that arrives with the grid).
        #[cfg(unix)]
        {
            let tid = ws.focused_buffer();
            if terminals.contains_key(&tid) {
                match ws.focused().view.mode() {
                    Mode::Terminal => {
                        if matches!(prefix, Prefix::TermEscape) {
                            *prefix = Prefix::None;
                            if is_ctrl(key, 'n') {
                                run_cmd(
                                    Command::EnterTerminalNormal,
                                    ws,
                                    files,
                                    recorded,
                                    status,
                                    quit,
                                );
                                return;
                            }
                            // Not the escape: the swallowed `CTRL-\` is dropped; fall through to send `key`.
                        }
                        if is_ctrl(key, '\\') {
                            *prefix = Prefix::TermEscape;
                            return;
                        }
                        if let Some(bytes) = pty::encode_key(key) {
                            if let Some(t) = terminals.get_mut(&tid) {
                                t.send(&bytes);
                            }
                        }
                        return;
                    }
                    // Only the plain keys resume: `<C-a>` / `<C-i>` are not `a` / `i` (#1069). Anything else
                    // falls through to the normal editor grammar (read-only over an empty doc).
                    Mode::TerminalNormal
                        if matches!(key.code, KeyCode::Char('i' | 'a' | 'A'))
                            && !crate::input::is_chord(key) =>
                    {
                        run_cmd(Command::EnterTerminal, ws, files, recorded, status, quit);
                        return;
                    }
                    _ => {}
                }
            }
        }
        // The focused pane's visible height (for the scroll / recenter commands below).
        let focused_h = rects
            .get(ws.focus())
            .map_or(text_rows as usize, |r| r.h as usize);
        // `zf{motion}` (Normal mode): keep buffering keys after `zf` until they resolve to a linewise
        // motion, then create a CLOSED fold over the line range the motion covers — sourced from the same
        // `reindent_range` seam `=` uses, so `zf` folds exactly the lines `=`/`d` would touch. The cursor
        // lands on the fold's first line (matches nvim). This is folds "slice 2" — the Normal-mode
        // counterpart of the Visual `zf` below.
        if let Prefix::Zf(pending) = prefix {
            let mut buf = std::mem::take(pending);
            *prefix = Prefix::None;
            // A char key extends the motion; Esc or any non-char key abandons the pending `zf`.
            let ch = match key.code {
                KeyCode::Char(c)
                    if (key.modifiers - crossterm::event::KeyModifiers::SHIFT).is_empty() =>
                {
                    Some(c)
                }
                _ => None,
            };
            let Some(c) = ch else {
                return; // cancelled — the `zf` prefix is already cleared
            };
            buf.push(c);
            match crate::folds::parse_fold_motion(&buf) {
                crate::folds::FoldMotionParse::More => {
                    *prefix = Prefix::Zf(buf); // still accumulating (a count or a multi-key motion)
                }
                crate::folds::FoldMotionParse::Cancel => {} // not a fold motion — drop it
                crate::folds::FoldMotionParse::Ready { motion, count } => {
                    // A one-line fold is a fold too (nvim): it never shows closed ('foldminlines').
                    if let Some((start, end)) = ws.reindent_range(motion, count) {
                        let fv = folds.entry(ws.focused_buffer()).or_default();
                        crate::folds::insert_closed_fold(fv, start, end);
                        place_cursor(ws, recorded, line_idx.nth_line_start(start));
                        status.echo(format!("folded {} lines", end - start + 1));
                    }
                }
            }
            return;
        }
        // `z` scroll prefix: the second key recenters the view on the cursor line (`zz`/`z.` center,
        // `zt`/`z<CR>` top, `zb`/`z-` bottom). Only `top` moves; the per-frame scroll pass then applies
        // `scrolloff`, so `zt`/`zb` land the line `scrolloff` rows inside the edge, as in Vim.
        if matches!(prefix, Prefix::Swallow) {
            *prefix = Prefix::None;
            return;
        }
        if let Prefix::Z(count) = *prefix {
            *prefix = Prefix::None;
            // Scroll-align verbs (zz/zt/zb/z./z-/z<CR>/z+/z^). `{count}` first moves the cursor to line
            // `count` (clamped to the last line): `zz`/`zt`/`zb` keep its column, the others put it on the
            // first non-blank, with or without a count (`:help z<CR>`). Neither is a jump.
            let to = match key.code {
                KeyCode::Char('z' | '.') => Some(viewport::RecenterTo::Center),
                KeyCode::Char('t' | '+') | KeyCode::Enter => Some(viewport::RecenterTo::Top),
                KeyCode::Char('b' | '-' | '^') => Some(viewport::RecenterTo::Bottom),
                _ => None,
            };
            if let Some(to) = to {
                let cur = line_idx.line_of(ws.focused().view.cursor().0);
                let last = line_idx.line_of(snapshot.len().saturating_sub(1));
                let top = ws.focused().view.top();
                let row = match (key.code, count) {
                    // `z+`: the line just below the window goes to the top (nvim's `nv_zet`).
                    (KeyCode::Char('+'), 0) => (top + focused_h).min(last),
                    // `z^`: the line just above the window goes to the bottom.
                    (KeyCode::Char('^'), 0) => top.saturating_sub(1),
                    // `{count}z^`: put line `count` at the bottom (keeping 'scrolloff'), then the line now at
                    // the top goes to the bottom.
                    (KeyCode::Char('^'), n) => {
                        let so = so.min(focused_h.saturating_sub(1) / 2);
                        (n as usize - 1)
                            .min(last)
                            .saturating_sub(focused_h.saturating_sub(1 + so))
                    }
                    (_, 0) => cur,
                    (_, n) => (n as usize - 1).min(last),
                };
                if matches!(key.code, KeyCode::Char('z' | 't' | 'b')) {
                    if row != cur {
                        let (n, m) = if row > cur {
                            (row - cur, ruse_core::Motion::Down)
                        } else {
                            (cur - row, ruse_core::Motion::Up)
                        };
                        run_cmd(
                            Command::Move(n as u32, m),
                            ws,
                            files,
                            recorded,
                            status,
                            quit,
                        );
                    }
                } else {
                    let start = line_idx.nth_line_start(row);
                    let blank = snapshot[start.min(snapshot.len())..]
                        .iter()
                        .take_while(|b| matches!(b, b' ' | b'\t'))
                        .count();
                    // An all-blank line keeps the cursor on its last blank, not on the newline.
                    let at_eol = matches!(snapshot.get(start + blank), Some(b'\n') | None);
                    let off = if at_eol {
                        blank.saturating_sub(1)
                    } else {
                        blank
                    };
                    place_cursor(ws, recorded, start + off);
                }
                // Over closed folds the rows are counted as nvim counts them (a fold is one row).
                let fv = folds
                    .get(&ws.focused_buffer())
                    .map_or(&[][..], Vec::as_slice);
                let new_top = match frame.wins.get(ws.focus()) {
                    Some(win) if fv.iter().any(|f| f.closed) => {
                        let lines = PaneLines {
                            text: PaneText {
                                bytes: snapshot,
                                idx: line_idx,
                            },
                            win,
                            folds: fv,
                            virt: &[],
                            height: focused_h,
                        };
                        viewport::recenter_lines(&lines, row, focused_h, to)
                    }
                    _ => viewport::recenter(row, focused_h, to),
                };
                ws.set_top(ws.focus(), new_top);
                return;
            }
            // Horizontal scroll (#615): `zh` / `zl` (`z<Left>` / `z<Right>`) `count` columns, `zH` / `zL`
            // half the window's width, `zs` / `ze` the cursor's column first / last, then the cursor
            // moves back inside the window if it left it. Nothing while the window wraps (nvim).
            let n = count.max(1) as usize;
            let half = rects.get(ws.focus()).map_or(0, |r| usize::from(r.w) / 2);
            let hcmd = match key.code {
                KeyCode::Char('h') | KeyCode::Left => Some(HScroll::Left(n)),
                KeyCode::Char('l') | KeyCode::Right => Some(HScroll::Right(n)),
                KeyCode::Char('H') => Some(HScroll::Left(n.saturating_mul(half))),
                KeyCode::Char('L') => Some(HScroll::Right(n.saturating_mul(half))),
                KeyCode::Char('s') => Some(HScroll::CursorFirst),
                KeyCode::Char('e') => Some(HScroll::CursorLast),
                _ => None,
            };
            if let Some(hcmd) = hcmd {
                let Some(win) = frame.wins.get(ws.focus()) else {
                    return;
                };
                let (cursor, mode) = (ws.focused().view.cursor().0, ws.focused().view.mode());
                let text = PaneText {
                    bytes: snapshot,
                    idx: line_idx,
                };
                let line = line_idx.line_of(cursor.min(snapshot.len()));
                let start = text.start(line);
                let lay = win.lay(text.line(line));
                let folded = folds
                    .get(&ws.focused_buffer())
                    .is_some_and(|fv| crate::folds::closed_range(fv, line).is_some());
                let scrolled = crate::ui::window_view::hscroll(
                    win,
                    &lay,
                    cursor.saturating_sub(start),
                    win.tab_end(mode),
                    folded,
                    hcmd,
                );
                if let Some((leftcol, to)) = scrolled {
                    ws.set_leftcol(ws.focus(), leftcol);
                    if let Some(b) = to {
                        place_cursor(ws, recorded, start + b);
                    }
                }
                return;
            }
            // `zp` / `zP` (Normal) paste like `p` / `P` without a block's trailing spaces; `zy` yanks like
            // `y` without a block's trailing white space: over the selection in Visual, and in Normal as the
            // `y` operator (its motions are never blockwise, so it is `y` there), with the count kept.
            if (key.modifiers - KeyModifiers::SHIFT).is_empty() {
                let visual = matches!(ws.focused().view.mode(), Mode::Visual { .. });
                match key.code {
                    KeyCode::Char(c @ ('p' | 'P')) if !visual => {
                        let cmd = Command::PasteBlockInner {
                            after: c == 'p',
                            count: count.max(1),
                        };
                        run_cmd(cmd, ws, files, recorded, status, quit);
                        return;
                    }
                    KeyCode::Char('y') if visual => {
                        let cmd = Command::VisualOp(ruse_core::VisualOp::YankTrim);
                        run_cmd(cmd, ws, files, recorded, status, quit);
                        return;
                    }
                    KeyCode::Char('y') => {
                        let mode = ws.focused().view.mode();
                        let digits = if count > 0 {
                            count.to_string()
                        } else {
                            String::new()
                        };
                        for d in digits.chars().chain(['y']) {
                            engine.feed(KeyEvent::new(KeyCode::Char(d), KeyModifiers::NONE), mode);
                        }
                        return;
                    }
                    _ => {}
                }
            }
            // Fold verbs (manual folds; F-007 carve-out). `zf` folds the Visual selection; the rest act on the fold at the cursor.
            let buf = ws.focused_buffer();
            let cur_line = line_idx.line_of(ws.focused().view.cursor().0);
            let fv = folds.entry(buf).or_default();
            match key.code {
                // `zf` — create a CLOSED fold. From a Visual selection: fold the selection's lines. From
                // Normal mode: arm the `zf{motion}` sub-state (handled at the top of the loop), which folds
                // the range the following motion covers (slice 2).
                // Under an indent 'foldmethod' `zf` is `E350` at once and no operator: the keys after it
                // run as typed (nvim's `foldManualAllowed`), and Visual mode stays.
                KeyCode::Char('f')
                    if fold_method.get(&buf).copied() == Some(crate::input::FoldMethod::Indent) =>
                {
                    status.error(E350);
                }
                KeyCode::Char('f') => {
                    if let Some((BytePos(s), BytePos(e))) =
                        ws.focused().view.selection_span(snapshot)
                    {
                        let (a, b) = (line_idx.line_of(s), line_idx.line_of(e.saturating_sub(1)));
                        let (start, end) = (a.min(b), a.max(b));
                        run_cmd(Command::EnterNormal, ws, files, recorded, status, quit);
                        crate::folds::insert_closed_fold(fv, start, end);
                        place_cursor(ws, recorded, line_idx.nth_line_start(start));
                        status.echo(format!("folded {} lines", end - start + 1));
                    } else {
                        // Normal mode: begin `zf{motion}`. Seed the buffer with any count typed before `z`
                        // (e.g. `3zfj`); further count digits typed after `f` are appended by the handler.
                        *prefix = Prefix::Zf(if count > 0 {
                            count.to_string()
                        } else {
                            String::new()
                        });
                    }
                }
                // `zo`/`zc`/`za`, `zO`/`zC`/`zA`, `zv`, `zx`/`zX` over nested folds (#1141), as nvim's
                // `setManualFold` walks the folds holding the cursor line from the outermost: `zo` opens
                // the outermost closed one (`{count}` levels), `zc` closes the innermost open one above the
                // first closed one, `zO` opens the outermost closed one and all inside it, `zC` closes
                // every one, `za`/`zA` open when a closed fold hides the line and close otherwise. No fold
                // at the line is `E490`. The cursor keeps its line inside a closed fold (nvim; #819).
                KeyCode::Char(c @ ('o' | 'c' | 'a' | 'O' | 'C' | 'A')) => {
                    let n = count.max(1);
                    let closed = crate::folds::is_closed_at(fv, cur_line);
                    let open = match c {
                        'o' | 'O' => true,
                        'c' | 'C' => false,
                        _ => closed,
                    };
                    let found = match (open, c.is_ascii_uppercase()) {
                        (true, true) => crate::folds::open_rec(fv, cur_line),
                        (false, true) => crate::folds::close_rec(fv, cur_line),
                        (true, false) => {
                            (0..n).fold(None, |f, _| f.or(crate::folds::open_one(fv, cur_line)))
                        }
                        (false, false) => {
                            (0..n).fold(None, |f, _| f.or(crate::folds::close_one(fv, cur_line)))
                        }
                    };
                    if found.is_none() {
                        status.error("E490: No fold found");
                    }
                }
                // `zj` / `zk` — move the cursor DOWN to the next fold's start / UP to the previous fold's
                // end (`{count}` repeats). A closed fold counts as one: the folds inside it are not
                // stepped to. Cursor-only, column 0 — matching nvim, which ignores 'startofline' here
                // (verified: `zj` lands at col 0 even with `startofline` on). Landing inside a closed fold
                // keeps the cursor there; it paints on the summary row.
                KeyCode::Char('j') | KeyCode::Char('k') => {
                    let n = count.max(1);
                    let down = matches!(key.code, KeyCode::Char('j'));
                    let mut line = cur_line;
                    let mut moved = false;
                    for _ in 0..n {
                        let next = if down {
                            crate::folds::next_fold_start(fv, line)
                        } else {
                            crate::folds::prev_fold_end(fv, line)
                        };
                        match next {
                            Some(l) => {
                                line = l;
                                moved = true;
                            }
                            None => break, // no more folds that way — keep the last landing
                        }
                    }
                    if moved {
                        place_cursor(ws, recorded, line_idx.nth_line_start(line));
                    }
                }
                // `zR`/`zM`/`zr`/`zm` set 'foldlevel' (the deepest nesting, 0, one more, one less; `{count}`
                // levels for `zr`/`zm`) and `zx`/`zX` re-apply it: a fold deeper than the level is closed,
                // the rest open (nvim's `newFoldLevel`, manual and indent folds alike). `zx` then opens the
                // folds holding the cursor line, as `zv`.
                KeyCode::Char(c @ ('R' | 'M' | 'r' | 'm' | 'x' | 'X')) => {
                    let maxl = crate::folds::max_level(fv);
                    let cur = foldlevel.get(&buf).copied().unwrap_or(0);
                    let n = count.max(1) as usize;
                    let new = match c {
                        'R' => maxl,
                        'M' => 0,
                        'r' => cur.saturating_add(n).min(maxl),
                        'm' => cur.saturating_sub(n),
                        _ => cur,
                    };
                    foldlevel.insert(buf, new);
                    let en = foldenable.get(&buf).copied().unwrap_or(true);
                    crate::folds::apply_foldlevel(fv, if en { new } else { usize::MAX });
                    if c == 'x' {
                        crate::folds::open_cursor_fold(fv, cur_line);
                    }
                }
                // `zd` deletes the innermost fold at the cursor line (or the closed one hiding it) and moves
                // the folds inside it up a level; `zD` deletes those too. `zE` deletes every fold. Folds an
                // indent 'foldmethod' makes cannot be deleted (`E351`/`E352`).
                KeyCode::Char(c @ ('d' | 'D' | 'E')) => {
                    let indent =
                        fold_method.get(&buf).copied() == Some(crate::input::FoldMethod::Indent);
                    if indent {
                        status.error(if c == 'E' {
                            "E352: Cannot erase folds with current 'foldmethod'"
                        } else {
                            "E351: Cannot delete fold with current 'foldmethod'"
                        });
                    } else if c == 'E' {
                        crate::folds::eliminate_all(fv);
                    } else if !crate::folds::delete_at(fv, cur_line, c == 'D') {
                        status.error("E490: No fold found");
                    }
                }
                // `zv` — view cursor line: open just enough folds to reveal the cursor line. Keeps the
                // folds, unlike `zd`.
                KeyCode::Char('v') => {
                    crate::folds::open_cursor_fold(fv, cur_line);
                }
                // A `z` command nvim has but ruse lacks (`zh`, `zs`, `zx`, spelling, …) stays a no-op; one
                // nvim lacks too beeps there (`nv_zet` → `clearopbeep`), stopping a macro or `:normal`
                // (#1111).
                KeyCode::Char(c)
                    if key.modifiers.is_empty()
                        && !"abcdefghijklmnoprstuvwxyzACDEFGHLMNOPRWX0123456789+-.^="
                            .contains(c) =>
                {
                    ws.beep();
                }
                _ => {}
            }
            return;
        }
        // F-007 window layer: a `C-w` prefix (Normal mode, no command-line, not Insert where `C-w`
        // deletes a word) arms the next key as a window command. A thin frontend intercept for MVP;
        // F-003's keymap router will absorb it into a proper layer.
        if let Prefix::Window(count) = *prefix {
            // A count may also follow `C-w` (`C-w 3 w`, as in nvim); a leading `0` is not a count.
            if let KeyCode::Char(d @ '0'..='9') = key.code {
                if key.modifiers.is_empty() && (d != '0' || count > 0) {
                    let digit = d.to_digit(10).unwrap_or(0);
                    *prefix = Prefix::Window(count.saturating_mul(10).saturating_add(digit));
                    return;
                }
            }
            *prefix = Prefix::None;
            dispatch_window(key, count, ws, quit, status);
            return;
        }
        // The command-line window (`q:`/`q/`/`q?`) owns the keystream while open, like the `:` line: suppress
        // every Normal-mode intercept below so its keys (`j`/`k`/`<CR>`/`<Esc>`/`<C-c>`) reach `engine.feed`
        // → `feed_cmdwin` untouched.
        // The Emacs profile is modeless: its view starts in `Mode::Normal` but none of these Vim Normal
        // intercepts are Emacs keys, so they never fire there (`C-u 3 x`, `C-f`, `C-e` as the first keys of
        // an Emacs session reach `feed_emacs`, #1156).
        let normal = !*emacs_profile
            && matches!(ws.focused().view.mode(), Mode::Normal)
            && engine.cmdline().is_none()
            && engine.cmdwin().is_none();
        // Visual only: in Select a typed `z` replaces the selection (#792).
        let visual = matches!(ws.focused().view.mode(), Mode::Visual { .. });
        // What the engine is waiting for decides who owns this key (#679, #850). The session's own Normal
        // keys below (`z`, `C-w`, `&`, the scroll chords, the pickers, `K`) fire only at a command boundary;
        // while the engine waits for an argument (`fz`, `rz`, `mz`, `"z`, `gM`, `]M`, a leader key) the key is
        // that argument and goes to the engine. None of them is a motion, so under an armed operator they
        // cancel it and do nothing (Vim's `checkclearop`), and the count typed before them is theirs, never
        // the next command's.
        let pending = engine.pending();
        let chord = normal && pending == Pending::Base;
        let cancels_op = normal && pending == Pending::Operator;
        // `[z` / `]z` — the engine holds `[`/`]` as a `BracketPrefix`; only the fold-nav second key `z` is a
        // frontend view move, everything else (`]p`, `[M`, …) stays the engine's. `[z` → the current open
        // fold's START, `]z` → its END, or the fold around it when the cursor is already there
        // ([`crate::folds::open_fold_edge`]). No open fold at the cursor → no-op (Vim beeps). Count is ignored (Vim: `[z`/`]z` take
        // none), so consume it via `take_count`, which also resets the engine's `BracketPrefix`.
        if let Pending::Bracket { open } = pending {
            if normal && key.code == KeyCode::Char('z') && key.modifiers.is_empty() {
                engine.take_count();
                let buf = ws.focused_buffer();
                let cur_line = line_idx.line_of(ws.focused().view.cursor().0);
                if let Some(fv) = folds.get(&buf) {
                    if let Some(target) = crate::folds::open_fold_edge(fv, cur_line, open) {
                        place_cursor(ws, recorded, line_idx.nth_line_start(target));
                    }
                }
                return;
            }
        }
        let plain_z = key.code == KeyCode::Char('z') && key.modifiers.is_empty();
        // The Native profile keeps `C-f`/`C-b` for its file / buffer pickers; under Vim they page-scroll.
        let native = *profile_name == "native";
        let page = page_key(key, native);
        let session_chord = plain_z
            || page.is_some()
            || (key.code == KeyCode::Char('&') && key.modifiers.is_empty())
            || ['w', 'p', 'l', 'b', 'f', 'd', 'u', 'e', 'y']
                .iter()
                .any(|&c| is_ctrl(key, c));
        if cancels_op && session_chord {
            engine.take_count();
            if plain_z || is_ctrl(key, 'w') {
                *prefix = Prefix::Swallow;
            }
            return;
        }
        if chord && is_ctrl(key, 'w') {
            *prefix = Prefix::Window(engine.take_count());
            return;
        }
        // F-014: `K` = hover, `<C-]>` = goto-definition (when the focused buffer has a live server; else
        // both fall through, unbound in the grammar). The coordinator sends the request.
        if lsp.on_normal_key(key, chord, ws, files, snapshot) {
            engine.take_count();
            return;
        }
        // `C-p` opens the command palette (F-004 #2), context-filtered to the focused view.
        if chord && is_ctrl(key, 'p') {
            engine.take_count();
            *overlay = Some(Overlay::Palette(palette::open(
                &focused_context(ws),
                engine,
            )));
            return;
        }
        // `C-l` opens the buffer-line fuzzy picker (F-013 NAT-3). Normal-only, so the Emacs profile's
        // non-modal C-l (recenter) is unaffected; C-l is unbound in the Vim/Native Normal grammar.
        if chord && is_ctrl(key, 'l') {
            engine.take_count();
            *overlay = Some(Overlay::Lines(line_picker::open(ws.focused().doc.bytes())));
            return;
        }
        // Native profile: `C-b` opens the buffer picker and `C-f` the fuzzy file finder (F-013 NAT-3). Normal
        // only, so the Emacs profile's non-modal C-b / C-f (backward-/forward-char) is unaffected. Under the
        // Vim profile they are Vim's page scroll (below).
        if chord && native && is_ctrl(key, 'b') {
            engine.take_count();
            *overlay = Some(Overlay::Buffers(buffer_picker::open(ws)));
            return;
        }
        if chord && native && is_ctrl(key, 'f') {
            engine.take_count();
            *overlay = Some(Overlay::Files(file_picker::open()));
            return;
        }
        // Page scroll (`C-f`/`C-b`, `<PageDown>`/`<PageUp>`, `<S-Down>`/`<S-Up>`, `<S-CR>`), in Normal and
        // Visual and, through the Insert arm below, in Insert. `{count}` scrolls that many pages.
        let selection_base = visual && pending == Pending::Base;
        let fv = folds
            .get(&ws.focused_buffer())
            .map_or(&[][..], Vec::as_slice);
        if let (Some(forward), true) = (page, chord || selection_base) {
            let count = engine.take_count().max(1) as usize;
            run_scroll(
                ScrollKey::Page { forward, count },
                ws,
                frame,
                line_idx,
                fv,
                so,
                focused_h,
                screen_want,
                scroll_valid,
                files,
                recorded,
                status,
                quit,
            );
            return;
        }
        // `z` arms the scroll/fold prefix with the count typed before it. In Normal (scroll + fold verbs) and
        // Visual (so `zf` folds the selection); plain `z` (no ctrl), never on the command line.
        if (chord || (visual && pending == Pending::Base)) && plain_z {
            *prefix = Prefix::Z(engine.take_count());
            return;
        }
        // `&` repeats the last `:s` on the current line, keeping its flags (nvim's `&-default` mapping). The
        // session owns the key so it can read the count; see [`repeat_substitute_lines`].
        if chord && key.code == KeyCode::Char('&') && key.modifiers.is_empty() {
            let count = engine.take_count().max(1) as usize;
            status.give(repeat_substitute_lines(
                ws,
                recorded,
                last_substitute.as_ref(),
                count,
                report,
            ));
            return;
        }
        // `C-d` / `C-u` scroll the view and move the cursor by the window's 'scroll' lines (default half the
        // pane). A count sets 'scroll' for the window first (capped at the pane height), as in Vim.
        if (chord || selection_base) && (is_ctrl(key, 'd') || is_ctrl(key, 'u')) {
            let count = engine.take_count() as usize;
            if count > 0 {
                ws.set_scroll(ws.focus(), count.min(focused_h.max(1)), focused_h);
            }
            let amount = ws.focused().view.scroll(focused_h);
            run_scroll(
                ScrollKey::Half {
                    forward: is_ctrl(key, 'd'),
                    amount,
                },
                ws,
                frame,
                line_idx,
                fv,
                so,
                focused_h,
                screen_want,
                scroll_valid,
                files,
                recorded,
                status,
                quit,
            );
            return;
        }
        // `C-e` / `C-y` scroll the view one line down / up, nudging the cursor only if it would leave the
        // scrolloff band. Frontend-only (viewport concern); `{count}` scrolls that many lines.
        if chord && (is_ctrl(key, 'e') || is_ctrl(key, 'y')) {
            let count = engine.take_count().max(1) as usize;
            run_scroll(
                ScrollKey::Lines {
                    up: is_ctrl(key, 'e'),
                    count,
                },
                ws,
                frame,
                line_idx,
                fv,
                so,
                focused_h,
                screen_want,
                scroll_valid,
                files,
                recorded,
                status,
                quit,
            );
            return;
        }
        // `i_CTRL-E` / `i_CTRL-Y` (Insert mode) — insert the char directly BELOW / ABOVE the caret at the
        // same column. The engine has no buffer, so resolve the adjacent-line char here and hand it to the
        // engine, which emits (and dot-records) a concrete `InsertChar`. `None` (short/absent adjacent
        // line) is a Vim no-op. Guarded by `insert_plain_text_ctx` so a pending Insert prefix (e.g.
        // `CTRL-R`'s register name) still owns the key. Normal-mode `C-e`/`C-y` scroll is handled above.
        // These Insert-mode intercepts are Vim keys too: an Emacs session is in Insert after its first
        // printable key, and there `C-e`, `C-y`, `C-n`, `C-p`, `C-l` and the page keys are Emacs commands
        // (#1156).
        let vim_insert = !*emacs_profile && matches!(ws.focused().view.mode(), Mode::Insert);
        if vim_insert && (is_ctrl(key, 'e') || is_ctrl(key, 'y')) && engine.insert_plain_text_ctx()
        {
            // An active completion cycle owns the key first: `CTRL-E` cancels it, `CTRL-Y` accepts (#805).
            if let Some(feed) = engine.complete_end(is_ctrl(key, 'e')) {
                if let Feed::Cmd(cmd) = feed {
                    run_cmd(cmd, ws, files, recorded, status, quit);
                }
                return;
            }
            let ch = ws.adjacent_line_char(is_ctrl(key, 'y'));
            if let Feed::Cmd(cmd) = engine.insert_copy_char(ch) {
                run_cmd(cmd, ws, files, recorded, status, quit);
            }
            return;
        }
        // `i_<PageDown>` / `i_<PageUp>` / `i_<S-Down>` / `i_<S-Up>` — page scroll from Insert, a caret jump
        // that breaks undo and restarts the `.` record like `i_<Down>` (nvim v0.12.6).
        if let (true, Some(forward)) = (vim_insert, insert_page_key(key)) {
            if engine.insert_plain_text_ctx() {
                // The jump breaks undo first (it changes no text, so the scroll below sees the same lines).
                run_cmd(engine.insert_page_jump(), ws, files, recorded, status, quit);
                run_scroll(
                    ScrollKey::Page { forward, count: 1 },
                    ws,
                    frame,
                    line_idx,
                    fv,
                    so,
                    focused_h,
                    screen_want,
                    scroll_valid,
                    files,
                    recorded,
                    status,
                    quit,
                );
                return;
            }
        }
        // `i_CTRL-N` / `i_CTRL-P` (Insert mode) — keyword completion from the CURRENT buffer (F-003). The
        // engine has no buffer, so on the FIRST key the frontend resolves the base + candidate set (nvim
        // scan order) and hands them to the engine, which caches them and cycles the selection; subsequent
        // keys just step the cached cycle. The engine emits (and dot-records) a `CompleteWord` swapping the
        // completion region. Guarded by `insert_plain_text_ctx` (a pending Insert prefix still owns its key).
        // Other completion sources / the popup menu are out of scope for this slice (deferred).
        if vim_insert && (is_ctrl(key, 'n') || is_ctrl(key, 'p')) && engine.insert_plain_text_ctx()
        {
            let forward = is_ctrl(key, 'n');
            let feed = if engine.completion_active() {
                engine.complete_cycle(forward)
            } else {
                let (base, cands) = ws.keyword_completion();
                engine.complete_start(base, cands, forward)
            };
            if let Feed::Cmd(cmd) = feed {
                run_cmd(cmd, ws, files, recorded, status, quit);
            }
            return;
        }
        // `i_CTRL-X CTRL-L` (Insert mode) — whole-line completion from the CURRENT buffer (F-003). `CTRL-X`
        // armed the submode in the engine (`insert_ctrl_x_pending`); this `CTRL-L` resolves the candidate
        // lines here (the engine has no buffer — the same split as `i_CTRL-N`) and starts the cycle. A bare
        // `CTRL-L` while a cycle is already active steps it FORWARD (nvim's whole-line continuation key);
        // `CTRL-N`/`CTRL-P` cycle it too (the block above). Deferred: `CTRL-X CTRL-N/CTRL-P` (local keyword)
        // and `CTRL-X CTRL-F` (filename) sources, and the popup menu — all out of scope for this slice.
        if vim_insert && is_ctrl(key, 'l') {
            if engine.insert_ctrl_x_pending() {
                let (base, cands) = ws.line_completion();
                if let Feed::Cmd(cmd) = engine.complete_line_start(base, cands) {
                    run_cmd(cmd, ws, files, recorded, status, quit);
                }
                return;
            }
            if engine.completion_active() && engine.insert_plain_text_ctx() {
                if let Feed::Cmd(cmd) = engine.complete_cycle(true) {
                    run_cmd(cmd, ws, files, recorded, status, quit);
                }
                return;
            }
            // Otherwise a lone `CTRL-L` in Insert with no active completion — fall through (unbound, no-op).
        }
        // `H` / `M` / `L` — the top / middle / bottom visible line (first non-blank via GotoLine). These are
        // viewport-dependent, so they stay a FRONTEND intercept: the session resolves the target buffer line
        // from the current viewport, then hands it to the engine, which composes it with any PENDING OPERATOR
        // over `GotoLine` (`dH`/`yL`/`c2M`/`>H`/`gUL`…). Operator-pending is the `op` axis WITHIN Normal mode
        // (not a distinct crossterm mode), so `normal` is still true after `d`/`c`/`y`/… and this intercept
        // fires for the operator case too — `screen_op` returns the operator command when armed, else the bare
        // move, preserving plain `H`/`M`/`L`. Under an operator `GotoLine` is LINEWISE in core (`:help H`), and
        // its range is `[min,max]` of the cursor and target lines, so direction is handled for free. While the
        // engine waits for an argument (`fH`, `rL`, `gM`, `]M`) the letter is that argument instead (#679).
        if (chord || cancels_op)
            && !key
                .modifiers
                .contains(crossterm::event::KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('H' | 'M' | 'L'))
        {
            let to = match key.code {
                KeyCode::Char('H') => viewport::ScreenTo::High,
                KeyCode::Char('M') => viewport::ScreenTo::Middle,
                _ => viewport::ScreenTo::Low,
            };
            // The effective count (`2H` = 2nd line from top; `3d2H` = 6H) resolves the target line; the op
            // itself is consumed by `screen_op`. `screen_count` applies the operator*motion multiplication.
            let count = engine.screen_count();
            // The last real line: the slot after a final newline is no line (#677), so `M` on a short
            // buffer takes the middle of its lines, not of one more.
            let last_line = line_idx.line_of(
                ruse_core::motion::off_phantom(snapshot, ruse_core::BytePos(snapshot.len())).0,
            );
            // Under an operator Vim IGNORES 'scrolloff' — `dH`/`dL` operate through the TRUE top/bottom visible
            // line (verified against nvim), whereas the bare cursor-motion keeps the scrolloff margin.
            let scrolloff = if engine.has_op() { 0 } else { so };
            // The lines that show whole: fewer than the rows when lines wrap (#615).
            let top = ws.focused().view.top();
            let shown = match frame.wins.get(ws.focus()) {
                Some(win) => {
                    let fv = folds
                        .get(&ws.focused_buffer())
                        .map_or(&[][..], Vec::as_slice);
                    let lines = PaneLines {
                        text: PaneText {
                            bytes: snapshot,
                            idx: line_idx,
                        },
                        win,
                        folds: fv,
                        virt: &[],
                        height: focused_h,
                    };
                    if fv.iter().any(|f| f.closed) {
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
            let row = match shown {
                Ok(shown) => viewport::screen_line(top, shown, scrolloff, to, count, last_line),
                Err(row) => row,
            };
            match engine.screen_op(row as u32 + 1) {
                Feed::Cmd(cmd) => run_cmd(cmd, ws, files, recorded, status, quit),
                // `!H` / `!L` / `!M`: the filter over the screen line opens its pre-seeded `:{range}!` line,
                // exactly like `!{motion}` below (#800) — it was dropped, so the command was typed as text.
                Feed::FilterMotion { count, motion } => {
                    if let Some((first, last)) = ws.reindent_range(motion, count) {
                        engine.open_filter_cmdline(&format!("{},{}!", first + 1, last + 1));
                    }
                }
                _ => {}
            }
            return;
        }
        // Every other key — command-line included — goes through the engine (F-026): the command-line
        // namespace owns its buffer, so the frontend no longer special-cases `:`/`/` typing.
        let fed = engine.feed(key, ws.focused().view.mode());
        if let Some(e) = engine.take_error() {
            status.error(e.to_string());
        }
        // A screen-line motion (#615) lands where the window's screen rows say; the engine finishes it.
        let fed = match fed {
            Feed::ScreenMotion { motion, count } => {
                let fv = folds
                    .get(&ws.focused_buffer())
                    .map_or(&[][..], Vec::as_slice);
                resolve_screen_motion(motion, count, ws, engine, frame, line_idx, fv, screen_want)
            }
            fed => fed,
        };
        match fed {
            // A finished `:`-line (F-026): parse + run it. `submit_search` already folded a `/`-line
            // into `Feed::Cmd` inside the engine, so the frontend only sees the ex case here.
            Feed::ExecuteEx(text) => {
                self.execute_ex(text, frame);
            }
            // `c_CTRL-R_CTRL-W` / `c_CTRL-R_CTRL-A`: splice the `<cword>` / `<cWORD>` under the BUFFER
            // cursor into the open command line. The engine owns the cmdline buffer but has no document, so
            // resolve the word here and hand it back. No string under the cursor → nothing spliced (nvim
            // inserts nothing and stays on the line). An incsearch preview refreshes on the next redraw,
            // which reads the updated `engine.cmdline()`.
            Feed::CmdlineInsertUnder { big } => {
                let word = if big {
                    ws.cbig_word_under_cursor()
                } else {
                    ws.cword_under_cursor()
                };
                if let Some(w) = word {
                    engine.cmdline_splice(&w);
                }
            }
            // `c_CTRL-R_CTRL-L`: the buffer line under the cursor, without its newline.
            Feed::CmdlineInsertLine => {
                let b = ws.focused().doc.bytes();
                let line = ruse_core::pos::line_of(b, ws.focused().view.cursor());
                let start = ruse_core::pos::nth_line_start(b, line).0.min(b.len());
                let end = b[start..]
                    .iter()
                    .position(|&c| c == b'\n')
                    .map_or(b.len(), |n| start + n);
                let text = String::from_utf8_lossy(&b[start..end]).into_owned();
                engine.cmdline_splice(&text);
            }
            // `<Tab>`, `<S-Tab>`, `c_CTRL-D`/`CTRL-L`/`CTRL-A` (#613): complete with what the workspace knows
            // (buffer names, `%`/`#`, option values); the engine owns the line and the wildmenu.
            Feed::CmdlineComplete(key) => {
                let buf = ws.focused_buffer();
                let ctx = ExCompletion {
                    ws,
                    files,
                    alt_file,
                    fold_method: fold_method.get(&buf).copied(),
                    foldlevel: foldlevel.get(&buf).copied(),
                };
                engine.cmdline_complete(key, &ctx);
            }
            // `c_CTRL-R {reg}`: the register's text at the caret. The read-only `"/ ": ". "%` registers are
            // refreshed first, as before any dispatch. An unknown or empty register splices nothing.
            Feed::CmdlineInsertRegister(name) => {
                sync_special_registers(
                    ws,
                    recorded,
                    engine,
                    &self.last_ex_line,
                    files,
                    &self.alt_file,
                );
                let name = name.to_ascii_lowercase();
                let linewise = ws
                    .register_snapshot()
                    .iter()
                    .any(|(n, kind, _)| *n == name && *kind == ruse_core::RegKind::Linewise);
                let bytes = ws.register_bytes(Some(name));
                engine.cmdline_splice_register(&String::from_utf8_lossy(&bytes), linewise);
            }
            // `!{motion}` / `!!` (the filter operator): the engine resolved a linewise motion but has no
            // document to number it. Resolve the op-span's first/last line here, then open the `:` cmdline
            // pre-seeded with `{first},{last}!` (Vim's `:.,.+N!`). Typing the command and `<CR>` runs it
            // through the SAME `:{range}!{cmd}` ex-filter path (parse_ex → Ex::Filter → shell.rs). An empty
            // span (nothing to filter) opens nothing, matching Vim leaving Normal untouched.
            Feed::FilterMotion { count, motion } => {
                if let Some((first, last)) = ws.reindent_range(motion, count) {
                    // `reindent_range` yields 0-based inclusive lines; the ex range is 1-based.
                    engine.open_filter_cmdline(&format!("{},{}!", first + 1, last + 1));
                }
            }
            // `!` over a search, `n`/`N`, `*`/`#`, `gn` or a mark (#800): the planner measures the span the
            // command's operator would cover; no target (no match, unset mark) opens nothing, as in nvim.
            Feed::FilterSpan(cmd) => {
                if !note_word_search(&cmd, ws, engine, search_hl, status) {
                    return;
                }
                // A search (`/pat`, `?pat`, `n`, `N`, bare, under an operator or `!`) whose pattern matches
                // nowhere fails with E486 and nothing runs (#901); with no previous pattern, E35 (#1173).
                if search_misses(&cmd, ws, status) {
                    return;
                }
                if no_previous_pattern(&cmd) {
                    status.error(E35);
                    return;
                }
                if let Some((first, last)) = ws.span_lines(&cmd) {
                    engine.open_filter_cmdline(&format!("{},{}!", first + 1, last + 1));
                }
            }
            // `{count}:` — the line opens on the cursor's line and the `count - 1` below it, numbered here.
            // nvim seeds `:.,.+{count-1}` (`:.` for 1); the lines are the same, and a range past the last line
            // is E16 when it runs, as there.
            Feed::CountCmdline { count } => {
                let first =
                    ruse_core::pos::line_of(ws.focused().doc.bytes(), ws.focused().view.cursor())
                        + 1;
                let last = first + count as usize - 1;
                engine.open_filter_cmdline(&if count == 1 {
                    format!("{first}")
                } else {
                    format!("{first},{last}")
                });
            }
            // Resolved above.
            Feed::Pending | Feed::Ignored | Feed::ScreenMotion { .. } => {}
            // An unknown key (#1111): record the failure, so a macro or `:normal` stops here.
            Feed::Beep { clear_register } => {
                if clear_register {
                    run_cmd(
                        Command::SetRegister(None),
                        ws,
                        files,
                        recorded,
                        status,
                        quit,
                    );
                }
                ws.beep();
            }
            // A change that fails before nvim saves its redo (`d/zzz<CR>`, `dfz`, `d'a` unset) is no change
            // for `.`: the engine recorded it before the core could tell (#1179). The beep is kept for the
            // macro and `:normal` abort read after the key.
            Feed::Cmd(cmd) => {
                let beeped_before = self.ws.take_beep();
                self.ws.take_refused();
                self.run_resolved_cmd(cmd.clone(), frame);
                let beeped = self.ws.take_beep();
                // A refused register beeps after nvim saved the redo; a failed motion before (#1184).
                let refused = self.ws.take_refused();
                if beeped {
                    self.engine.change_failed(&cmd, refused);
                }
                if beeped_before || beeped {
                    self.ws.beep();
                }
            }
            // `` g`a `` / `g'a` (#1175): the jump runs under `keepjumps`, switched through the trace (RFC-0020
            // R6) as `:keepjumps` is, so a replay skips the jumplist entry too. Already on (`:keepjumps normal
            // g`a`), it stays on.
            Feed::KeepJumps(cmd) => {
                let kept = self.ws.keep_jumps();
                if !kept {
                    run_ex(ExCmd::KeepJumps(true), &mut self.ws, &mut self.recorded);
                }
                self.run_resolved_cmd(cmd, frame);
                if !kept {
                    run_ex(ExCmd::KeepJumps(false), &mut self.ws, &mut self.recorded);
                }
            }
            // `.` (dot-repeat) replays the last change; record and apply each concrete command so the
            // trace (F-022) captures the resolved edit, not the `.` keypress.
            Feed::Replay(cmds) => {
                sync_special_registers(ws, recorded, engine, last_ex_line, files, alt_file);
                // `.` over a closed fold acts on it as the typed command did (#819): its first command
                // meets the buffer as it is now; the rest follow from where that one leaves it.
                let fv = folds
                    .get(&ws.focused_buffer())
                    .map_or(&[][..], Vec::as_slice);
                let mut cmds = cmds.into_iter();
                let first = cmds.next().and_then(|c| {
                    crate::app::fold_cmds::prepare(c, ws, fv, line_idx, snapshot, recorded)
                });
                // Python re-indents on the replayed keys as on the typed ones (nvim's redo re-types them).
                let can = &mut self.can_cindent;
                let highlighters = &self.highlighters;
                if let Some(first) = first {
                    for cmd in std::iter::once(first).chain(cmds) {
                        let after = crate::app::indent_keys::before(&cmd, ws, highlighters, can);
                        crate::app::dispatch::run_cmd_reported(
                            cmd, ws, files, recorded, status, quit, report,
                        );
                        if let Some(fix) = crate::app::indent_keys::after(after, ws, highlighters) {
                            run_cmd(fix, ws, files, recorded, status, quit);
                        }
                    }
                }
            }
            Feed::Repeat(r) => {
                sync_special_registers(ws, recorded, engine, last_ex_line, files, alt_file);
                run_repeat(r, ws, files, recorded, status, quit, report);
            }
        }
    }

    /// Run a finished engine `Command` the way a typed key's is run (#689): the frontend-resolved ones
    /// (`&`/`g&`, the INFO commands, `[i`/`[I`, `*`/`#`, a missed search, `gd`, tree-aware `=`) are resolved
    /// here, hlsearch and the special registers are refreshed, then it is recorded and applied. The
    /// command palette dispatches through this too, so a command picked there behaves as when typed.
    fn run_resolved_cmd(&mut self, cmd: Command, frame: &FrameCtx) {
        let report = self.ui_opts.report;
        let Session {
            ws,
            files,
            highlighters,
            line_idx,
            recorded,
            engine,
            quit,
            search_hl,
            last_substitute,
            last_ex_line,
            overlay,
            status,
            alt_file,
            folds,
            can_cindent,
            key_typed,
            normal_depth,
            ui_opts,
            ..
        } = self;
        let snapshot = &frame.snapshot;
        // `g&` — repeat the last `:s` over the WHOLE FILE with its flags. Resolved here (the engine
        // has no substitute history); like the current-line `&`, it calls `substitute` directly.
        if matches!(cmd, Command::RepeatSubstituteGlobal) {
            status.give(match &last_substitute {
                Some((pat, rep, flags)) => {
                    // Recorded as the resolved `:%s` it repeats (RFC-0020); a match is a jump (#1119).
                    let range = ruse_core::SubRange::WholeFile;
                    let ex = substitute_cmd(ws, range, pat, rep, *flags, None);
                    match sub_outcome(run_ex(ex, ws, recorded)) {
                        Ok(out) if out.replacements == 0 => {
                            Msg::error(format!("E486: Pattern not found: {pat}"))
                        }
                        Ok(out) => crate::app::dispatch::substituted_message(&out, report),
                        Err(e) => Msg::error(crate::app::dispatch::regex_error_msg(&e)),
                    }
                }
                None => Msg::error("E33: No previous substitute regular expression"),
            });
            return;
        }
        // `&` picked by its command id (the palette): the typed `&` with no count.
        if matches!(cmd, Command::RepeatSubstituteLine) {
            status.give(repeat_substitute_lines(
                ws,
                recorded,
                last_substitute.as_ref(),
                1,
                report,
            ));
            return;
        }
        // INFO commands (`ga`/`:ascii`, `CTRL-G`, `g CTRL-G`): print to the status line, no buffer
        // mutation. Like `*`/`gd`, the engine has no buffer, so the frontend reads the focused
        // buffer's bytes + cursor here and formats the message via the pure `ruse_core::info` helpers.
        if let Some(msg) = info_status(&cmd, ws) {
            status.give(msg);
            return;
        }
        // `[i`/`]i`/`[I`/`]I` (keyword under cursor): the engine has no buffer, so resolve the
        // keyword here and scan the buffer lines. `[i`/`]i` echo the count'th match on the status
        // line; `[I`/`]I` open a view-only listing overlay. Purely a display command (never planned).
        // No keyword under the cursor → `E349` (like nvim; note `*`/`gd` use `E348`, a different code).
        if let Command::ShowKeywordLines { above, list, count } = cmd {
            match ws.word_under_cursor() {
                Some(keyword) => {
                    let pane = ws.focused();
                    let bytes = pane.doc.bytes();
                    let cursor_line = ruse_core::pos::line_of(bytes, pane.view.cursor()) + 1;
                    let matches = ruse_core::keyword_line_numbers(bytes, &keyword);
                    if list {
                        let rows: Vec<(usize, String)> =
                            ruse_core::keyword_list(&matches, cursor_line, above)
                                .into_iter()
                                .map(|l| (l, ruse_core::line_text(bytes, l)))
                                .collect();
                        status.echo(format!("{} line(s) matching \"{keyword}\"", rows.len()));
                        *overlay = Some(Overlay::Listing(crate::ui::keyword_lines_picker::open(
                            &rows,
                        )));
                    } else {
                        status.give(
                            match ruse_core::keyword_echo(
                                &matches,
                                cursor_line,
                                above,
                                count as usize,
                            ) {
                                ruse_core::KeywordEcho::Line(l) => {
                                    Msg::echo(ruse_core::line_text(bytes, l))
                                }
                                ruse_core::KeywordEcho::CurrentLine => {
                                    Msg::error("E387: Match is on current line")
                                }
                                ruse_core::KeywordEcho::NotFound => {
                                    Msg::error("E389: Couldn't find pattern")
                                }
                            },
                        );
                    }
                }
                None => status.error("E349: No identifier under cursor"),
            }
            return;
        }
        // `*`/`#` (word under cursor): the planner resolves the keyword and searches; record the same
        // pattern as the last search (direction too, so `#` then `n` continues backward) and for
        // hlsearch. No keyword on the line → E348 and nothing runs.
        if !note_word_search(&cmd, ws, engine, search_hl, status) {
            return;
        }
        // A search (`/pat`, `?pat`, `n`, `N`, bare, under an operator or `!`) whose pattern matches
        // nowhere fails with E486 and nothing runs (#901). The core fails it too (a beep, nothing moved or
        // edited); it still runs so the wanted column resets, as nvim's `normal_search` sets
        // `w_set_curswant` before it searches (#1171).
        // `n`/`N`/`/<CR>` before any search fail with E35 (#1173). The core fails the empty pattern (a beep,
        // nothing moved); it runs, recorded, so the wanted column resets as nvim's `normal_search` resets it.
        if no_previous_pattern(&cmd) {
            run_cmd(cmd, ws, files, recorded, status, quit);
            status.error(E35);
            return;
        }
        if search_misses(&cmd, ws, status) {
            if matches!(cmd, Command::Search { .. }) {
                let err = status.take();
                run_cmd(cmd, ws, files, recorded, status, quit);
                status.restore(err);
            }
            return;
        }
        // `gd`/`gD` (declaration under cursor, TEXT heuristic — not LSP): like `*`/`#`, the engine
        // has no buffer, so resolve the keyword here and rewrite to a concrete whole-file first-match
        // jump. `gd` and `gD` are currently identical (matching nvim v0.12.4). Set the last-search to
        // the whole-word pattern forward so a following `n` continues from the landing spot (nvim's
        // `gd` sets `@/` too). `global` is unused for now — the `gd` block-scope refinement reads it.
        let cmd = if let Command::GotoDeclaration { .. } = cmd {
            match ws.word_under_cursor() {
                Some(word) => {
                    let pat = format!("\\<{word}\\>");
                    engine.set_last_search(pat.clone(), true);
                    Command::GotoFirstMatch(pat)
                }
                None => {
                    status.error("E348: No string under cursor");
                    return;
                }
            }
        } else {
            cmd
        };
        // Tree-aware `=` (F-015): if the focused buffer has a live syntax tree, resolve the reindent
        // Over closed folds (#819): `j`/`k` count a fold as one line, an operator takes it whole, `o`/`p`
        // open and put past it. Before the tree-aware rewrite, which reads the cursor line `o` opens from.
        let fv = folds
            .get(&ws.focused_buffer())
            .map_or(&[][..], Vec::as_slice);
        let Some(cmd) = crate::app::fold_cmds::prepare(cmd, ws, fv, line_idx, snapshot, recorded)
        else {
            return;
        };
        // 'foldopen' applies to typed keys only (nvim checks `KeyTyped`): not to a macro, `:normal` or a
        // `:g` of them. A shift opens the fold whatever 'foldopen' holds (`op_shift`).
        let typed = *key_typed && *normal_depth == 0;
        let opens_fold = crate::app::fold_cmds::shift_opens_fold(&cmd)
            || (typed && crate::app::fold_cmds::opens_fold(&cmd, &ui_opts.foldopen));
        // `gq` under 'formatprg' / `=` under 'equalprg' filter through the program instead (#617, #618).
        if crate::app::dispatch::run_prg_operator(&cmd, ws, recorded, status) {
            return;
        }
        let indent_after = crate::app::indent_keys::before(&cmd, ws, highlighters, can_cindent);
        let cmd = resolve_tree_aware(cmd, ws, highlighters, line_idx, snapshot);
        // A completed search turns on hlsearch for that pattern (F-009 #1).
        if let Some(p) = search_pattern(&cmd) {
            *search_hl = Some(p);
        }
        // Refresh the read-only special registers `"/ ": ". "%` before dispatch so a `"/p` /
        // `C-r :` / `".p` / `"%p` in this command reads the live values (`:help quote_/`).
        sync_special_registers(ws, recorded, engine, last_ex_line, files, alt_file);
        crate::app::dispatch::run_cmd_reported(cmd, ws, files, recorded, status, quit, report);
        // Python re-indents the line its 'indentkeys' and opens leave (#1205, #1206).
        if let Some(fix) = crate::app::indent_keys::after(indent_after, ws, highlighters) {
            run_cmd(fix, ws, files, recorded, status, quit);
        }
        // 'foldopen': a search, a horizontal move, a mark, undo… open the fold the cursor lands in.
        if opens_fold {
            if let Some(fv) = folds.get_mut(&ws.focused_buffer()) {
                let pane = ws.focused();
                crate::folds::open_at(
                    fv,
                    ruse_core::pos::line_of(pane.doc.bytes(), pane.view.cursor()),
                );
            }
        }
    }

    /// Run a finished `:`-line (F-026): split it at its `|` separators (`:help :bar`) and run each command
    /// in turn. As in Vim, a command that fails (an `E…:` status, `E492` if unknown) drops the rest of the
    /// line, and so do a quit and an interactive `:s///c` (which owns the keys until it ends).
    fn execute_ex(&mut self, text: String, frame: &FrameCtx) {
        // An empty line (`:<CR>`, `:<C-u><CR>`) does nothing, as in Vim; it is no unknown command.
        if text.trim_matches([' ', '\t', ':']).is_empty() {
            return;
        }
        // Record the executed command-line for Vim's `":` register (`:help quote_:`), without the
        // leading `:` (the engine already strips it). Read by `C-r :` / `":p` / `@:`.
        self.last_ex_line = Some(text.clone());
        self.run_ex_pieces(crate::input::split_bar(&text), frame);
        if let Some(start) = self.pending_insert.take() {
            self.start_or_stop_insert(start);
        }
    }

    /// Run a `:startinsert`-family command (or `:stopinsert` for `None`) once its line has run (#609), the
    /// way Vim's `restart_edit` starts Insert after the command line.
    fn start_or_stop_insert(&mut self, start: Option<(crate::input::StartKind, bool)>) {
        use crate::input::StartKind;
        let Session {
            ws,
            files,
            recorded,
            status,
            quit,
            ..
        } = self;
        let mode = ws.focused().view.mode();
        let Some((kind, bang)) = start else {
            if mode != Mode::Normal {
                // Only `i_CTRL-O` runs an Ex command in Insert mode; its return is dropped, and the
                // cursor stays where `CTRL-O` left it (no `<Esc>` step back) unless it sat past the end.
                let at = ws.focused().view.cursor().0;
                run_cmd(Command::EnterNormal, ws, files, recorded, status, quit);
                let b = ws.focused().doc.bytes();
                if at < ruse_core::pos::line_end(b, ruse_core::BytePos(at)).0 {
                    place_cursor(ws, recorded, at);
                }
            }
            return;
        };
        if mode != Mode::Normal {
            return;
        }
        let enter = match kind {
            StartKind::Insert if bang => Command::AppendLineEnd,
            StartKind::Insert => Command::EnterInsert,
            StartKind::Replace => Command::EnterReplace,
            StartKind::VirtualReplace => Command::EnterVirtualReplace,
        };
        if bang && kind != StartKind::Insert {
            // `:startreplace!` replaces from the end of the line: enter at the line's end, as `A` does.
            run_cmd(Command::AppendLineEnd, ws, files, recorded, status, quit);
            run_cmd(Command::EnterNormal, ws, files, recorded, status, quit);
            let pane = ws.focused();
            let b = pane.doc.bytes();
            let end = ruse_core::pos::line_end(b, pane.view.cursor()).0;
            place_cursor(ws, recorded, end);
        }
        run_cmd(enter, ws, files, recorded, status, quit);
    }

    /// Run the `|`-separated commands of one line in turn (see [`Session::execute_ex`]). A backwards range
    /// stops the line at its prompt; the commands after it wait in the prompt for its answer.
    fn run_ex_pieces(&mut self, pieces: Vec<String>, frame: &FrameCtx) {
        let mut pieces = pieces.into_iter();
        while let Some(cmd) = pieces.next() {
            self.status.clear();
            // Command modifiers (`:silent`, `:verbose`, …) come first (#609). `:silent` hides the command's
            // message but not an error; `:silent!` hides the error too, and the line goes on after it. The
            // hidden error is still the last error (Vim's `v:errmsg`).
            let (mods, cmd) = crate::input::strip_modifiers(&cmd);
            if mods.filter.is_some() && cmd.trim().is_empty() {
                self.status.error("E476: Invalid command");
                break;
            }
            // `:keeppatterns`, `:keepalt` and `:keepjumps` (#609): put back what the command changed.
            let kept_patterns = mods.keep_patterns.then(|| {
                (
                    self.engine.search_state(),
                    self.search_hl.clone(),
                    self.last_substitute.clone(),
                )
            });
            let kept_alt = mods
                .keep_alt
                .then(|| (self.ws.alternate(), self.alt_file.clone()));
            // The switch goes through the trace (RFC-0020 R6): the command it wraps replays without the
            // jumps it skipped live.
            let jumps_were_kept = self.ws.keep_jumps();
            if mods.keep_jumps && !jumps_were_kept {
                run_ex(ExCmd::KeepJumps(true), &mut self.ws, &mut self.recorded);
            }
            self.ex_filter = mods.filter.clone();
            self.ex_verbose = mods.verbose;
            // What `:silent` keeps off the screen stays out of `:messages` too (#1235).
            let hushed = self.status.hush(if mods.silent_errors {
                Silence::All
            } else if mods.silent {
                Silence::Messages
            } else {
                Silence::Off
            });
            self.execute_ex_one(cmd.to_string(), frame);
            self.status.unhush(hushed);
            self.ex_filter = None;
            self.ex_verbose = false;
            if !jumps_were_kept && self.ws.keep_jumps() {
                run_ex(ExCmd::KeepJumps(false), &mut self.ws, &mut self.recorded);
            }
            if let Some((search, hl, sub)) = kept_patterns {
                self.engine.restore_search_state(search);
                self.search_hl = hl;
                self.last_substitute = sub;
            }
            if let Some((alt, alt_file)) = kept_alt {
                self.ws.set_alternate(alt);
                self.alt_file = alt_file;
            }
            if let Some(prompt) = &mut self.swap {
                prompt.pending.extend(pieces);
                return;
            }
            if mods.silent_errors && self.status.is_error() {
                self.silenced_error = Some(self.status.take().text);
            } else if mods.silent && !self.status.is_error() {
                self.status.clear();
            }
            if self.quit || self.confirm.is_some() || self.status.is_error() {
                break;
            }
        }
    }

    /// The key that answered a backwards-range prompt (`ask_yesno` in nvim): `y` swaps the range and runs
    /// the rest of the line, `n`, `<Esc>` or `CTRL-C` drop the whole line, and any other key is ignored
    /// while the prompt stays up.
    fn answer_swap(&mut self, prompt: SwapPrompt, key: KeyEvent, frame: &FrameCtx) {
        let plain = !key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
        match key.code {
            KeyCode::Char('y') if plain => {
                self.status.clear();
                self.run_ex_pieces(prompt.pending, frame);
            }
            KeyCode::Char('n') | KeyCode::Esc if plain => self.status.clear(),
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.status.clear()
            }
            _ => self.swap = Some(prompt),
        }
    }

    /// Run ONE Ex command (a `|`-free piece of the typed line). This is the single Ex dispatch (#892):
    /// every `Ex` variant has exactly one arm here, so a new command cannot land in a match that never sees
    /// it.
    fn execute_ex_one(&mut self, text: String, frame: &FrameCtx) {
        let parsed = parse_ex(&text);
        let external = runs_external(&parsed);
        if external {
            self.sync_journals();
        }
        self.execute_ex_parsed(text, frame);
        // The files may have changed: after another program ran (nvim checks after a shell command), and
        // after a write, which may have been refused because one did (RFC-0023 §8). `:checktime` asks.
        self.check_disk(external || parsed == Ex::CheckTime);
    }

    fn execute_ex_parsed(&mut self, text: String, frame: &FrameCtx) {
        let Session {
            ws,
            files,
            highlighters,
            initial,
            start_options,
            start_registers,
            trace_watch,
            recorded,
            profile_name,
            emacs_profile,
            engine,
            quit,
            term,
            folds,
            fold_method,
            foldlevel,
            foldenable,
            fold_rev,
            confirm,
            swap,
            search_hl,
            hlsearch_on,
            incsearch_on,
            fixeol_on,
            scrolloff,
            ui_opts,
            opt_sources,
            ex_verbose,
            config,
            last_substitute,
            last_shell,
            overlay,
            #[cfg(unix)]
            terminals,
            lsp,
            status,
            alt_file,
            prev_dir,
            ex_filter,
            pending_insert,
            rescue,
            journals,
            journal_writers,
            readonly,
            global_busy,
            ..
        } = self;
        let (cols, text_rows) = (frame.cols, frame.text_rows);
        let (snapshot, rects) = (&frame.snapshot, &frame.rects);
        // The current, alternate and numbered file names, for `%` / `#` / `#n` in a file argument or shell
        // command.
        let file_name = |id: Option<DocumentId>| {
            id.and_then(|id| files.get(&id))
                .map(|bf| bf.path.to_string_lossy().into_owned())
        };
        let (cur_name, alt_name) = (
            file_name(Some(ws.focused_buffer())),
            crate::app::ex_more::alternate_name(alt_file, ws, files),
        );
        let numbered: Vec<(u64, String)> = files
            .iter()
            .map(|(id, bf)| (id.0, bf.path.to_string_lossy().into_owned()))
            .collect();
        let names = crate::input::FileNames {
            current: cur_name.as_deref(),
            alternate: alt_name.as_deref(),
            buffers: &numbered,
        };
        let expand = |arg: &str| crate::input::expand_filename(arg, &names);
        let expand_opt = |arg: Option<String>| arg.map(|a| expand(&a)).transpose();
        // A shell command: `%`/`#` first, then `!` → the previous command; remembered for the next `!`.
        let mut shell_cmd = |cmd: &str| -> Result<String, crate::input::ExpandError> {
            let cmd = crate::input::expand_bang(&expand(cmd)?, last_shell.as_deref())?;
            *last_shell = Some(cmd.clone());
            Ok(cmd)
        };
        // The leading range (`.`, `$`, offsets, marks, `/pat/`, `'<,'>` from Visual `:`) resolves to line
        // numbers (`app::ex_range`); one past the buffer is E16 (#1079), a range with no command goes to its
        // line (`:5`, `:$`), and a backwards one asks before it is swapped.
        // `:[count]messages [clear]` (#1235): its count is no line number, so it is read before the range.
        if let Some(cmd) = crate::app::messages::parse_messages(&text) {
            match cmd {
                Ok(cmd) if cmd.clear => status.history_mut().keep_last(cmd.count.unwrap_or(0)),
                Ok(cmd) => {
                    let shown: Vec<&str> = status.history().last(cmd.count).collect();
                    let shown = shown.join("\n");
                    status.echo(shown);
                }
                Err(e) => status.error(e.to_string()),
            }
            return;
        }
        let pats = crate::app::ex_range::AddrPatterns {
            search: engine.last_search(),
            substitute: last_substitute.as_ref().map(|(p, _, _)| p.as_str()),
        };
        // Inside a `:g` a range does not grow to closed folds (nvim checks `global_busy`).
        let range_folds = (!*global_busy)
            .then(|| folds.get(&ws.focused_buffer()).map(Vec::as_slice))
            .flatten();
        let resolved = crate::app::ex_range::resolve_ex_range(&text, ws, pats, range_folds);
        // A `/pat/` or `?pat?` address becomes the last search, as in Vim (`:/foo/d` then `n`).
        if let Some((pat, forward)) = resolved.search {
            engine.set_last_search(pat.clone(), forward);
            *search_hl = Some(pat);
        }
        // A `;` in the range moved the cursor, and it stays moved whatever the command does (`:5;-2d`).
        if let Some(line) = resolved.cursor {
            goto_line(line, ws, files, recorded, status, quit);
        }
        let text = match resolved.range {
            Ok(ExRange::Line(text)) => text,
            Ok(ExRange::Backwards(swapped)) => {
                status.echo("Backwards range given, OK to swap (y/n)?");
                *swap = Some(SwapPrompt {
                    pending: vec![swapped],
                });
                return;
            }
            Ok(ExRange::Goto(line)) => {
                goto_line(line, ws, files, recorded, status, quit);
                return;
            }
            Err(e) => {
                status.error(e.to_string());
                return;
            }
        };
        let ex = match parse_ex(&text) {
            Ex::Edit(spec) => {
                expand_opt(spec.file.clone()).map(|file| Ex::Edit(EditSpec { file, ..spec }))
            }
            Ex::Find { split, spec } => expand_opt(spec.file.clone()).map(|file| Ex::Find {
                split,
                spec: EditSpec { file, ..spec },
            }),
            Ex::Drop(spec) => {
                expand_opt(spec.file.clone()).map(|file| Ex::Drop(EditSpec { file, ..spec }))
            }
            Ex::SaveAs { bang, file, opts } => {
                expand(&file).map(|file| Ex::SaveAs { bang, file, opts })
            }
            Ex::Write(spec) => match &spec.target {
                WriteTarget::File(f) => expand_opt(f.clone()).map(WriteTarget::File),
                WriteTarget::Append(f) => expand_opt(f.clone()).map(WriteTarget::Append),
                WriteTarget::Command(c) => shell_cmd(c).map(WriteTarget::Command),
            }
            .map(|target| Ex::Write(WriteSpec { target, ..spec })),
            Ex::Read {
                addr,
                source: ReadSource::File(path),
            } => expand(&path).map(|p| Ex::Read {
                addr,
                source: ReadSource::File(p),
            }),
            Ex::Read {
                addr,
                source: ReadSource::Command(cmd),
            } => shell_cmd(&cmd).map(|c| Ex::Read {
                addr,
                source: ReadSource::Command(c),
            }),
            Ex::Filter { range, cmd } => shell_cmd(&cmd).map(|cmd| Ex::Filter { range, cmd }),
            Ex::Shell(cmd) => shell_cmd(&cmd).map(Ex::Shell),
            ex => Ok(ex),
        };
        let ex = match ex {
            Ok(ex) => ex,
            Err(e) => {
                status.error(e.to_string());
                return;
            }
        };
        // An Ex line a command asks to run after it (`:e +cmd`).
        let mut then_run = None;
        let mut trust_change = None;
        let mut config_reload = false;
        match ex {
            Ex::NoHighlight => *search_hl = None, // `:noh` clears the search highlight (F-009 #1)
            // `:set` / `:setlocal` / `:setglobal` (#614): the grammar is `input/set_cmd.rs`, the values it
            // reads and writes are behind `set_exec::OptionHost`.
            Ex::SetOptions(line) => {
                let win_height = frame.rects.get(ws.focus()).map_or(0, |r| usize::from(r.h));
                let mut host = crate::app::set_exec::OptionHost {
                    ws,
                    recorded,
                    engine,
                    hlsearch_on,
                    search_hl,
                    incsearch_on,
                    fixeol_on,
                    scrolloff,
                    folds,
                    fold_method,
                    foldlevel,
                    foldenable,
                    fold_rev,
                    sources: opt_sources,
                    ui: ui_opts,
                    files,
                    readonly,
                    win_height,
                };
                let out =
                    crate::app::set_exec::run_set(&line, &mut host, cols as usize, *ex_verbose);
                status.history_mut().set_cap(ui_opts.message_history());
                crate::app::ex_more::show_rows(out.rows, status, overlay);
                if let Some(e) = out.error {
                    status.error(e);
                }
            }
            // `:fmt` / `:rename {new}` / `:references` / `:codeaction` (F-014): the coordinator sends
            // the request; the response is dispatched + applied (or opens a picker) on a later frame.
            ex @ (Ex::Format | Ex::Rename(_) | Ex::References | Ex::CodeAction) => {
                lsp.on_ex(&ex, ws, files, snapshot, status);
            }
            // `:trust` (D-058): the user grants workspace trust for this session; language servers
            // start on the next frame's sync. It also records the content-bound trust of the workspace
            // config (D-062, #614), and `:untrust` removes that record (both below, after this match).
            Ex::Trust => {
                lsp.grant_trust(status);
                trust_change = Some(true);
            }
            Ex::Untrust => trust_change = Some(false),
            // `:config-reload` (Helix's name, #1152): read the config layers again now (below, after this
            // match); it also leaves safe mode.
            Ex::ConfigReload => config_reload = true,
            // `:inspect config [key]` (CFG-PROV-001): the layers, their state, every value a layer set
            // with its source, and the diagnostics; or one key's layer stack.
            Ex::InspectConfig(key) => {
                let rows = match key.as_deref() {
                    None => Some(crate::config::inspect_all(config)),
                    Some(k) => crate::config::inspect_key(config, k),
                };
                match rows {
                    Some(rows) => crate::app::ex_more::show_rows(rows, status, overlay),
                    None => {
                        status.error(format!("unknown config key: {}", key.unwrap_or_default()));
                    }
                }
            }
            // `:diagnostics` (F-014): open a picker over the focused buffer's already-collected
            // diagnostics (no server round-trip); Enter jumps to the selected one.
            Ex::Diagnostics => {
                let diags = lsp.diagnostics_for(ws.focused_buffer());
                if diags.is_empty() {
                    status.echo("no diagnostics");
                } else {
                    status.echo(format!("{} diagnostic(s)", diags.len()));
                    *overlay = Some(Overlay::Diagnostics(diag_picker::open(diags, snapshot)));
                }
            }
            // `:registers` (F-029): open a view-only picker over the non-empty registers, so a
            // recorded macro (`"a`) or a yank is inspectable.
            Ex::Registers => {
                let snapshot = ws.register_snapshot();
                if snapshot.is_empty() {
                    status.echo("no registers set");
                } else {
                    status.echo(format!("{} register(s)", snapshot.len()));
                    *overlay = Some(Overlay::Registers(register_picker::open(snapshot)));
                }
            }
            // `:ascii`/`:as` — the ex synonym of `ga`: print the char-under-cursor's value.
            Ex::Ascii => {
                let pane = ws.focused();
                status.echo(ruse_core::ascii_info(pane.doc.bytes(), pane.view.cursor()));
            }
            // `:digraphs` / `:dig`: open the view-only digraph-listing overlay.
            Ex::Digraphs => {
                let p = crate::ui::digraph_picker::open();
                status.echo(format!("{} digraph(s)", p.rows().len()));
                *overlay = Some(Overlay::Digraphs(p));
            }
            // `:marks` (F-003): open a picker over the set marks; Enter jumps to the selected one.
            Ex::Marks => {
                let snapshot = ws.marks_snapshot();
                if snapshot.is_empty() {
                    status.echo("no marks set");
                } else {
                    status.echo(format!("{} mark(s)", snapshot.len()));
                    *overlay = Some(Overlay::Marks(marks_picker::open(
                        snapshot.into_iter().map(|(c, p)| (c, p.0)).collect(),
                        ws.focused().doc.bytes(),
                    )));
                }
            }
            // `:[range]mark {x}` / `:[range]k{x}` (#1088).
            Ex::Mark { range, name } => {
                let line = match range {
                    ruse_core::SubRange::WholeFile => {
                        crate::app::dispatch::line_count(ws.focused().doc.bytes())
                    }
                    range => ws.resolve_range(range).1,
                };
                let mark = ExCmd::Mark {
                    line,
                    name,
                    nonblank: true,
                };
                run_ex(mark, ws, recorded);
            }
            // `:jumps` / `:changes` (F-003): position viewers over the jumplist / change list;
            // Enter jumps. Both use the position-viewer overlay.
            ex @ (Ex::Jumps | Ex::Changes) => {
                let positions = if matches!(ex, Ex::Jumps) {
                    ws.jumps_snapshot()
                } else {
                    ws.changes_snapshot()
                };
                if positions.is_empty() {
                    status.echo("empty");
                } else {
                    status.echo(format!("{} position(s)", positions.len()));
                    *overlay = Some(Overlay::Positions(pos_picker::open(
                        positions.into_iter().map(|p| p.0).collect(),
                        ws.focused().doc.bytes(),
                    )));
                }
            }
            // `:terminal` (F-011): spawn a shell in a new PTY-backed buffer, sized to the focused
            // window, and enter Terminal mode. Unix-only in slice 1.
            Ex::Terminal => {
                #[cfg(unix)]
                {
                    let rect = rects.get(ws.focus());
                    let cols = rect.map_or(cols, |r| r.w).max(1);
                    let rows = rect.map_or(text_rows, |r| r.h).max(1);
                    match term_buffer::Terminal::spawn(rows, cols) {
                        Ok(term) => {
                            let id = ws.add_buffer(Vec::new(), Some("[terminal]".to_string()));
                            ws.focus_buffer(id);
                            terminals.insert(id, term);
                            run_cmd(Command::EnterTerminal, ws, files, recorded, status, quit);
                            status.echo("terminal started (CTRL-\\ CTRL-N to leave)");
                        }
                        Err(e) => status.error(format!("E: cannot start terminal: {e}")),
                    }
                }
                #[cfg(not(unix))]
                {
                    status.error("terminal is not supported on this platform");
                }
            }
            // Lang-Arg map maintenance (F-027): the map is engine state.
            Ex::Lmap { lhs, rhs } => engine.set_lang_mapping(lhs, rhs),
            Ex::Lunmap { lhs } => engine.clear_lang_mapping(lhs),
            // `:[range]normal[!] {keys}` replays the key payload through the input `engine` → the same
            // `Command` pipeline as typed keys / macro replay.
            Ex::Normal { range, keys, .. } => self.ex_normal(range, &keys, frame),
            // `:[range]g/pat/cmd` (F-009 #4): two-pass mark-then-execute over the focused window. A bare `d`
            // and a plain `s` run in core; a `normal` payload replays keys per marked line through the
            // engine, and any other command runs per marked line through this dispatch (#1245).
            // An empty `:g//` pattern is the last search, and the `:g` pattern becomes the last search
            // before the command runs, so `:g/pat/s//X/` substitutes `pat` (Vim, #804).
            mut ex @ Ex::Global(_) => {
                if let Err(msg) = crate::input::reuse_last_search(&mut ex, engine.last_search()) {
                    status.error(msg.to_string());
                    return;
                }
                let Ex::Global(spec) = ex else {
                    unreachable!("bound by the arm's pattern")
                };
                // A `:g` inside a `:g` (Vim's `global_busy`) takes no range but the whole file (E147), and
                // runs its command on the current line only, when that line matches.
                let nested = *global_busy;
                let range = if nested {
                    let b = ws.focused().doc.bytes();
                    let whole = match spec.range {
                        ruse_core::SubRange::WholeFile => true,
                        ruse_core::SubRange::Lines(1, n) => {
                            n == crate::app::dispatch::line_count(b).max(1)
                        }
                        _ => false,
                    };
                    if !whole {
                        status.error("E147: Cannot do :global recursive with a range");
                        return;
                    }
                    let line = ruse_core::pos::line_of(b, ws.focused().view.cursor()) + 1;
                    ruse_core::SubRange::Lines(line, line)
                } else {
                    spec.range
                };
                engine.set_last_search_pattern(spec.pattern.clone());
                *search_hl = Some(spec.pattern.clone());
                // Vim reports a pattern `:g` cannot use as `E476` (`ex_global`).
                let marked = match ws.global_marks(range, &spec.pattern, spec.negate) {
                    Ok(marked) => marked,
                    Err(e) => {
                        status.error(crate::app::dispatch::invalid_command_msg(&e));
                        return;
                    }
                };
                // Not an error in Vim: `:g` with no match only echoes the message (no `E486`), and a
                // nested one says nothing.
                let Some(&last_marked) = marked.last() else {
                    if !nested {
                        status.info(format!("Pattern not found: {}", spec.pattern));
                    }
                    return;
                };
                match &spec.cmd {
                    crate::input::GlobalPayload::Core(cmd) => {
                        // A `:g/pat/s///g` reports its substitutions (`4 substitutions on 2 lines`): the
                        // matches on the marked lines, counted before it runs. Without `g` it is one per
                        // line the `:s` changed.
                        let global_subs =
                            match cmd {
                                ruse_core::GlobalCmd::Substitute { pattern, flags, .. }
                                    if flags.global =>
                                {
                                    let span = ruse_core::SubRange::Lines(
                                        marked.first().copied().unwrap_or(1),
                                        last_marked,
                                    );
                                    let on: HashSet<usize> = marked.iter().map(|m| m - 1).collect();
                                    ws.substitute_preview(span, pattern, "", *flags).ok().map(
                                        |subs| subs.iter().filter(|s| on.contains(&s.line)).count(),
                                    )
                                }
                                _ => None,
                            };
                        // Recorded as the lines the pattern marked and the command (RFC-0020).
                        let ex = ExCmd::Global {
                            marks: marked,
                            cmd: cmd.clone(),
                        };
                        let lines_before =
                            crate::app::dispatch::line_count(ws.focused().doc.bytes());
                        let result = match run_ex(ex, ws, recorded) {
                            ExOutcome::Failed(e) => Err(e),
                            out => Ok(lines_outcome(&out)),
                        };
                        // The `:s` that ran on the marked lines set the last search in turn, even when it
                        // changed nothing (#1104).
                        if let ruse_core::GlobalCmd::Substitute { pattern, .. } = cmd {
                            engine.set_last_search_pattern(pattern.clone());
                            *search_hl = Some(pattern.clone());
                        }
                        match result {
                            // The inner `:s` matched nowhere: no message, and the cursor rests at the start
                            // of the last marked line, where `:g` put it to run the command (nvim v0.12.6).
                            Ok(0) => {
                                let at = crate::app::ex_more::line_start(
                                    ws.focused().doc.bytes(),
                                    last_marked,
                                );
                                place_cursor(ws, recorded, at);
                                status.clear();
                            }
                            // nvim's `:g` report: the lines `:d` removed (`msgmore`), or the substitutions
                            // the `:s` made.
                            Ok(n) => match cmd {
                                ruse_core::GlobalCmd::Delete
                                    if ws.focused().doc.bytes().is_empty() =>
                                {
                                    status.echo("--No lines in buffer--");
                                }
                                ruse_core::GlobalCmd::Delete => give_report(
                                    status,
                                    report::line_delta(
                                        lines_before,
                                        crate::app::dispatch::line_count(ws.focused().doc.bytes()),
                                        ui_opts.report,
                                    ),
                                ),
                                ruse_core::GlobalCmd::Substitute { .. } => give_report(
                                    status,
                                    report::substituted(
                                        global_subs.unwrap_or(n),
                                        n,
                                        ui_opts.report,
                                    ),
                                ),
                            },
                            Err(e) => status.error(crate::app::dispatch::invalid_command_msg(&e)),
                        }
                    }
                    // `:g/pat/p` (#609): show the marked lines; the cursor ends on the last one.
                    crate::input::GlobalPayload::Print(flags) => {
                        let keep = match crate::app::ex_more::filter_keep(ws, ex_filter.as_ref()) {
                            Ok(keep) => keep,
                            Err(e) => {
                                status.error(regex_error_msg(&e));
                                return;
                            }
                        };
                        let pane = ws.focused();
                        let b = pane.doc.bytes();
                        let ts = pane.view.tabs();
                        let rows: Vec<String> = marked
                            .iter()
                            .flat_map(|&n| crate::app::ex_print::rows(b, n, n, *flags, ts, &*keep))
                            .collect();
                        let at = crate::app::ex_more::line_start(b, last_marked);
                        place_cursor(ws, recorded, at);
                        crate::app::ex_more::show_rows(rows, status, overlay);
                    }
                    crate::input::GlobalPayload::Normal { keys, .. } => {
                        self.ex_global_normal(&marked, keys, frame);
                    }
                    crate::input::GlobalPayload::Ex(cmd) => self.ex_global_ex(&marked, cmd, frame),
                }
            }
            // `:{range}fold` (#1141): a closed manual fold over the range, as `zf`; the cursor stays.
            Ex::Fold(range) => {
                let buf = ws.focused_buffer();
                if fold_method.get(&buf).copied() == Some(crate::input::FoldMethod::Indent) {
                    status.error(E350);
                    return;
                }
                let (a, b) = ws.resolve_range(range);
                let fv = folds.entry(buf).or_default();
                crate::folds::insert_closed_fold(fv, a.max(1) - 1, b.max(1) - 1);
            }
            // `:{range}foldopen[!]` / `:{range}foldclose[!]` (#1141): `zo`/`zc` (`zO`/`zC`) on each line of
            // the range, past each closed fold; `E490` when no fold holds any of its lines.
            Ex::FoldOpen {
                range,
                open,
                recursive,
            } => {
                let (a, b) = ws.resolve_range(range);
                let fv = folds.entry(ws.focused_buffer()).or_default();
                if !crate::folds::op_range(fv, a.max(1) - 1, b.max(1) - 1, open, recursive) {
                    status.error("E490: No fold found");
                }
            }
            // `:{range}folddoopen {cmd}` / `:{range}folddoclosed {cmd}` (#1141): run `{cmd}` on each line of
            // the range not in (or in) a closed fold, through the per-line dispatch `:g` uses (nvim's
            // `ex_folddo` marks the lines, then runs `global_exe`).
            Ex::FoldDo { range, closed, cmd } => {
                let (a, b) = ws.resolve_range(range);
                let fv = folds
                    .get(&ws.focused_buffer())
                    .map_or(&[][..], Vec::as_slice);
                let marked: Vec<usize> = (a.max(1)..=b)
                    .filter(|&l| crate::folds::closed_range(fv, l - 1).is_some() == closed)
                    .collect();
                if !marked.is_empty() {
                    self.ex_global_ex(&marked, &cmd, frame);
                }
            }
            // `:checkhealth` (F-030): gather the frontend snapshot (guard/profile/highlighter) and render
            // the report's one-line summary into status.
            Ex::CheckHealth => {
                let inputs = health::HealthInputs {
                    profile: profile_name,
                    caret: if *emacs_profile {
                        "between-char"
                    } else {
                        "on-char"
                    },
                    sgr_mouse: term.sgr_mouse,
                    sync_output: term.sync_output,
                    bracketed_paste: term.bracketed_paste,
                    focus_events: term.focus_events,
                    file_ext: files
                        .get(&ws.focused_buffer())
                        .and_then(|bf| bf.path.extension())
                        .map(|e| e.to_string_lossy().into_owned()),
                    grammar_ok: highlighters.contains_key(&ws.focused_buffer()),
                    buffers: ws.window_count(),
                    trace_commands: recorded.len(),
                    journaling: journal_health(*rescue, *journals, files, ws, journal_writers),
                    config: crate::config::health_issue(config),
                };
                // Every check's line in the listing, the summary on the status line (RFC-0023 §8).
                crate::app::ex_more::show_rows(
                    health::lines(&health::report(&inputs)),
                    status,
                    overlay,
                );
            }
            // `:earlier`/`:later` walk chronological undo time by recording + applying N of the
            // g-/g+ commands (done HERE so they land in `recorded` for trace fidelity).
            // `:checktime`: the look itself follows the command ([`Session::execute_ex_one`]).
            Ex::CheckTime => {}
            Ex::Earlier(n) => time_travel(true, n, ws, files, recorded, status, quit),
            Ex::Later(n) => time_travel(false, n, ws, files, recorded, status, quit),
            // `:e[!] [++ff] [+cmd] [{file}]` (F-007, #610): open a file into a buffer (shared with the file
            // picker), or re-read the focused buffer's own file, which unsaved changes block (`E37`) without `!`.
            // A `+cmd` runs once the file is open (after this dispatch, which holds the session's fields).
            Ex::Edit(spec) => {
                if crate::app::file_cmds::ex_edit(&spec, ws, files, highlighters, status) {
                    then_run = spec
                        .cmd
                        .as_deref()
                        .map(crate::app::file_cmds::plus_cmd_line);
                }
            }
            // `:[addr]r {file}` / `:[addr]r !{cmd}` — read a file's contents (pure IO) or a shell
            // command's stdout below the addressed line. The read is IMPURE (fs / process) and stays in
            // the frontend; core only splices the resulting bytes. The cursor lands on the
            // FIRST inserted line for a file read, the LAST for a command read (nvim v0.12.4 quirk).
            Ex::Read { addr, source } => match source {
                ReadSource::File(path) => match persist::read_file(std::path::Path::new(&path)) {
                    Ok(raw) => {
                        // Strip the read file's own BOM / CRLF as opening it would; the target
                        // buffer's format is applied on save (#860).
                        let clean = persist::encoding::FileFormat::normalize(&raw);
                        let n = read_text(ws, recorded, addr, clean.to_vec(), false);
                        status.echo(format!("\"{path}\" {n}L, {}B", raw.len()));
                    }
                    Err(e) => {
                        crate::log::open_failed("read", std::path::Path::new(&path), &e);
                        status.error(format!("E484: can't open {path}: {e}"));
                    }
                },
                ReadSource::Command(cmd) => match crate::shell::capture(&cmd) {
                    Ok(out) => {
                        let n = read_text(ws, recorded, addr, out.text.clone().into_bytes(), true);
                        let lines = if n == 1 {
                            "1 more line".to_string()
                        } else {
                            format!("{n} more lines")
                        };
                        status.info(with_shell_failure(&out, lines));
                    }
                    Err(e) => status.error(e.to_string()),
                },
            },
            // `:{range}!{cmd}` — FILTER: pipe the range's lines through the shell command and replace
            // them with its output. Pull the range text out FIRST (read-only), shell out, then splice.
            // As in nvim the output (stdout + stderr) replaces the range even when the command fails,
            // and the status line says `shell returned {n}` (#861).
            Ex::Filter { range, cmd } => match ws.range_text(range) {
                Some(input) => match crate::shell::filter(&cmd, &input) {
                    Ok(out) => {
                        // Recorded with the filter's output as data: replay never runs the command.
                        let (first, last) = ws.resolve_range(range);
                        let ex = ExCmd::FilterReplace {
                            first,
                            last,
                            bytes: out.raw.clone(),
                        };
                        let n = lines_outcome(&run_ex(ex, ws, recorded));
                        let lines = if n <= 1 {
                            String::new()
                        } else {
                            format!("{n} lines filtered")
                        };
                        status.info(with_shell_failure(&out, lines));
                    }
                    Err(e) => status.error(e.to_string()),
                },
                None => status.error("E749: empty buffer"),
            },
            // `:!{cmd}` — run the command and show its output on the status line; NO buffer change.
            // Multi-line output is collapsed to one status line (a scrolling message pane is a
            // follow-up); an empty result echoes the command so the user sees it ran.
            Ex::Shell(cmd) => match crate::shell::capture(&cmd) {
                Ok(out) => {
                    let joined = out.text.trim_end_matches('\n').replace('\n', " | ");
                    let shown = if joined.is_empty() {
                        format!(":!{cmd}")
                    } else {
                        joined
                    };
                    status.echo(match out.failure() {
                        Some(note) => format!("{shown} | {note}"),
                        None => shown,
                    });
                }
                Err(e) => status.error(e.to_string()),
            },
            // `:bd` deletes the focused buffer and drops its `files`/highlighter entries. Guards unsaved
            // changes with E89 unless `!` forces it.
            Ex::BufferDelete { force } => {
                let id = ws.focused_buffer();
                if !force && ws.focused().doc.is_modified() {
                    status.error("E89: No write since last change (add ! to override)");
                } else {
                    ws.remove_buffer(id);
                    // Its changes go with it (`:bd!`), as nvim deletes the swap file (#862).
                    if let Some(bf) = files.remove(&id) {
                        persist::journal::discard(Some(bf.path.as_path()));
                    }
                    highlighters.remove(&id);
                    status.info(format!("buffer {} deleted", id.0));
                }
            }
            // Bare `:s` / `:s {flags}` / `:&` / `:&&` — repeat the last `:s` against the frontend's
            // substitute history (the core keeps none), exactly like `&`/`g&`. `flags = None` (`:&&`)
            // KEEPS the stored flags; `flags = Some(f)` REPLACES them (bare `:s`/`:&` = default flags,
            // `:s g` = the given flags). Verified vs nvim v0.12.4.
            Ex::RepeatSubstitute { range, flags } => {
                status.give(match &last_substitute {
                    Some((pat, rep, stored)) => {
                        let use_flags = flags.unwrap_or(*stored);
                        let ex = substitute_cmd(ws, range, pat, rep, use_flags, None);
                        match sub_outcome(run_ex(ex, ws, recorded)) {
                            Ok(out) if out.lines == 0 && *global_busy => Msg::echo(""),
                            Ok(out) if out.lines == 0 => {
                                Msg::error(format!("E486: Pattern not found: {pat}"))
                            }
                            // A `:s/\n//` join on the last line: nothing to join, not an error.
                            Ok(out) if out.replacements == 0 => Msg::echo(""),
                            Ok(out) => {
                                crate::app::dispatch::substituted_message(&out, ui_opts.report)
                            }
                            Err(e) => Msg::error(crate::app::dispatch::regex_error_msg(&e)),
                        }
                    }
                    None => Msg::error("E33: No previous substitute regular expression"),
                });
            }
            // `:[range]s/pat/rep/flags` (F-009 #2). An empty pattern reuses the last search (`/`/`*`), as
            // Vim does, and a non-empty one is remembered so `&` can repeat it. It also becomes the last
            // search pattern (Vim's `save_re_pat`): `n` finds it next, and `:g//` or `:s//` reuse it.
            mut ex @ Ex::Substitute(_) => {
                if let Err(msg) = crate::input::reuse_last_search(&mut ex, engine.last_search()) {
                    status.error(msg.to_string());
                    return;
                }
                let Ex::Substitute(mut spec) = ex else {
                    unreachable!("bound by the arm's pattern")
                };
                // A count after the flags acts on that many lines from the range's last one (#1104).
                if let Some(n) = spec.count {
                    let line2 = crate::app::dispatch::resolve_normal_range(ws, spec.range).1;
                    let total = crate::app::dispatch::line_count(ws.focused().doc.bytes());
                    spec.range =
                        ruse_core::SubRange::Lines(line2, line2.saturating_add(n - 1).min(total));
                }
                // `&` starts from the previous substitute's flags; the `g` given here toggles its `g`.
                let stored = last_substitute
                    .as_ref()
                    .map(|(_, _, f)| *f)
                    .unwrap_or_default();
                let flags = if spec.keep_flags {
                    ruse_core::SubFlags {
                        global: stored.global != spec.global,
                        ignore_case: spec.ignore_case.or(stored.ignore_case),
                    }
                } else {
                    ruse_core::SubFlags {
                        global: spec.global,
                        ignore_case: spec.ignore_case,
                    }
                };
                if !spec.pattern.is_empty() {
                    *last_substitute =
                        Some((spec.pattern.clone(), spec.replacement.clone(), flags));
                    engine.set_last_search_pattern(spec.pattern.clone());
                    *search_hl = Some(spec.pattern.clone());
                }
                if spec.count_only {
                    // `n`: REPORT ONLY — count the matches and echo the tally, editing nothing (Vim `:s///n`).
                    // Overrides `c` (like Vim). `substitute_count` borrows `&`, so no edit / undo / cursor move.
                    status.give(
                        match ws.substitute_count(spec.range, &spec.pattern, flags) {
                            Ok(out) if out.replacements == 0 && *global_busy => Msg::echo(""),
                            Ok(out) if out.replacements == 0 => {
                                Msg::error(format!("E486: Pattern not found: {}", spec.pattern))
                            }
                            Ok(out) => Msg::info(match_count_message(out.replacements, out.lines)),
                            Err(e) => Msg::error(crate::app::dispatch::invalid_command_msg(&e)),
                        },
                    );
                } else if spec.confirm {
                    // `c`: compute the matches and hand control to the interactive confirm loop (F-009 #2).
                    match ws.substitute_preview(spec.range, &spec.pattern, &spec.replacement, flags)
                    {
                        Ok(subs) if subs.is_empty() => {
                            status.error(format!("E486: Pattern not found: {}", spec.pattern));
                        }
                        Ok(subs) => {
                            // The confirm loop records the resolved `:s` with the answers when it ends;
                            // that pushes the jump from where the cursor is now (nvim's `do_sub`, #1119).
                            let ex = substitute_cmd(
                                ws,
                                spec.range,
                                &spec.pattern,
                                &spec.replacement,
                                flags,
                                Some(Vec::new()),
                            );
                            let from = ws.focused().view.cursor().0;
                            *confirm = Some(Confirm::new(subs, ex, from));
                        }
                        Err(e) => status.error(crate::app::dispatch::invalid_command_msg(&e)),
                    }
                } else {
                    // Recorded resolved (RFC-0020); a `:s` that matched is a jump (#940).
                    let ex = substitute_cmd(
                        ws,
                        spec.range,
                        &spec.pattern,
                        &spec.replacement,
                        flags,
                        None,
                    );
                    status.give(match sub_outcome(run_ex(ex, ws, recorded)) {
                        // Nor inside a `:g`, which says nothing for the lines `:s` misses (Vim's `do_sub`).
                        Ok(out) if out.lines == 0 && (spec.quiet || *global_busy) => Msg::echo(""),
                        Ok(out) if out.lines == 0 => {
                            Msg::error(format!("E486: Pattern not found: {}", spec.pattern))
                        }
                        // A `:s/\n//` join on the last line: nothing to join, not an error.
                        Ok(out) if out.replacements == 0 => Msg::echo(""),
                        Ok(out) => match spec.print {
                            // `p`/`#`/`l`: show the last substituted line, where the cursor is.
                            Some(pflags) => {
                                let pane = ws.focused();
                                let b = pane.doc.bytes();
                                let line = ruse_core::pos::line_of(b, pane.view.cursor()) + 1;
                                crate::app::ex_print::rows(
                                    b,
                                    line,
                                    line,
                                    pflags,
                                    pane.view.tabs(),
                                    &|_| true,
                                )
                                .pop()
                                .map_or_else(|| Msg::echo(""), Msg::echo)
                            }
                            None => crate::app::dispatch::substituted_message(&out, ui_opts.report),
                        },
                        Err(e) => Msg::error(crate::app::dispatch::invalid_command_msg(&e)),
                    });
                }
            }
            // The `:w` family (#610). `:wq`/`:x` close the window only after a successful write (#842): no
            // file, or a failed write, keeps it open; `!` also lets the focused buffer's changes go.
            Ex::Write(spec) => {
                let id = ws.focused_buffer();
                let had_file = files.contains_key(&id);
                let ok = crate::app::file_cmds::ex_write(
                    &spec,
                    ws,
                    files,
                    highlighters,
                    *fixeol_on,
                    status,
                );
                // Writing a buffer that has a file to another name makes that name the alternate file
                // (Vim's 'cpoptions' `A`, #610).
                if let (true, true, WriteTarget::File(Some(name))) = (ok, had_file, &spec.target) {
                    let other =
                        crate::app::dispatch::buffer_for_path(files, std::path::Path::new(name));
                    if other != Some(id) {
                        *alt_file = Some(crate::app::ex_more::AltFile::new(ws, name.clone()));
                    }
                }
                if ok && spec.quit {
                    quit_window(ws, status, quit, spec.bang);
                }
            }
            Ex::SaveAs { bang, file, opts } => {
                let id = ws.focused_buffer();
                let old = files.get(&id).map(|bf| bf.path.display().to_string());
                let ok = crate::app::file_cmds::ex_saveas(
                    bang,
                    &file,
                    opts,
                    ws,
                    files,
                    highlighters,
                    *fixeol_on,
                    status,
                );
                // The buffer's old name becomes the alternate file (Vim's 'cpoptions' `A`, #610).
                let renamed = files.get(&id).map(|bf| bf.path.display().to_string());
                if let Some(old) = old.filter(|o| ok && Some(o) != renamed.as_ref()) {
                    *alt_file = Some(crate::app::ex_more::AltFile::new(ws, old));
                }
            }
            // `:wa` / `:wqa` / `:xa`: write every modified buffer with a file (a terminal has none to write).
            Ex::WriteAll { quit: then_quit } => {
                #[cfg(unix)]
                let skip = |id: DocumentId| terminals.contains_key(&id);
                #[cfg(not(unix))]
                let skip = |_: DocumentId| false;
                let ok = crate::app::file_cmds::ex_write_all(ws, files, *fixeol_on, status, skip);
                if ok && then_quit {
                    *quit = may_quit(ws, status, false);
                }
            }
            Ex::Quit => quit_window(ws, status, quit, false),
            Ex::QuitForce => quit_window(ws, status, quit, true),
            Ex::QuitAll { force: true } => *quit = true,
            Ex::QuitAll { force: false } => *quit = may_quit(ws, status, false),
            Ex::CQuit(code) => {
                crate::app::dispatch::set_exit_status(code);
                *quit = true;
            }
            Ex::Hide | Ex::Close => {
                if !ws.close_focused() {
                    status.error("E444: Cannot close last window");
                }
            }
            // F-007 window commands: split the focused window onto the same buffer, or close it.
            Ex::Split => {
                ws.split(SplitDir::Horizontal);
                status.echo("window split");
            }
            Ex::VSplit => {
                ws.split(SplitDir::Vertical);
                status.echo("window vsplit");
            }
            Ex::Only => {
                status.echo(match ws.only() {
                    0 => "already one window".into(),
                    1 => "1 window closed".into(),
                    n => format!("{n} windows closed"),
                });
            }
            Ex::SaveTrace(p) => {
                let trace = Trace::record_with_options(
                    initial,
                    start_options,
                    start_registers,
                    recorded.to_vec(),
                );
                // A trace that would not replay to this document is not written (#690). The trace holds
                // register contents and pasted clipboard text as well as the edits (RFC-0020 R6, Security
                // impact), so the message says so.
                let checked =
                    crate::app::trace_check::check(&trace, initial, ws, files, trace_watch)
                        .map_err(|r| r.to_string());
                match checked.and_then(|()| {
                    std::fs::write(&p, trace.to_text())
                        .map_err(|e| format!("trace save failed: {e}"))
                }) {
                    Ok(()) => status.info(format!(
                        "trace saved: {p} ({} commands; includes registers and clipboard text)",
                        recorded.len()
                    )),
                    Err(e) => status.error(e),
                }
            }
            // `:[range]d` — delete the range's lines (no range = the current line) as one undo group.
            // `:d`, `:>` and `:<` are jumps from where the cursor stood (nvim's `ex_operators`, #1119).
            Ex::Delete(range, reg, flags) => {
                let (first, last) = ws.resolve_range(range);
                let n = lines_outcome(&run_ex(ExCmd::Delete { first, last, reg }, ws, recorded));
                match flags {
                    // `:dl`/`:dp` (#1241): show the new current line, as `ex_may_print` does.
                    Some(flags) => {
                        let pane = ws.focused();
                        let b = pane.doc.bytes();
                        let line = ruse_core::pos::line_of(b, pane.view.cursor()) + 1;
                        let tab_width = pane.view.tabs();
                        status.echo(
                            crate::app::ex_print::rows(b, line, line, flags, tab_width, &|_| true)
                                .pop()
                                .unwrap_or_default(),
                        );
                    }
                    // nvim says this instead of the count when the buffer is left empty.
                    None if ws.focused().doc.bytes().is_empty() => {
                        status.echo("--No lines in buffer--");
                    }
                    None => give_report(status, report::more(-to_i64(n), ui_opts.report)),
                }
            }
            // `:[range]y` — yank the range's lines linewise into the unnamed register (like `yy`).
            Ex::Yank(range, reg) => {
                let (first, last) = ws.resolve_range(range);
                let n = lines_outcome(&run_ex(ExCmd::Yank { first, last, reg }, ws, recorded));
                give_report(status, report::yanked(n, false, reg, ui_opts.report));
            }
            // `:[range]j[oin][!]` — join the range's lines into one, reusing the core `J`/`gJ` join (no range =
            // the current line, which joins with the next). `bang` is the raw `gJ` form.
            Ex::Join { range, bang } => {
                let (first, last) = ws.resolve_range(range);
                let ex = ExCmd::Join {
                    first,
                    last,
                    no_space: bang,
                };
                // nvim's `:j` gives no report, whatever the count.
                run_ex(ex, ws, recorded);
                status.clear();
            }
            // `:[range]m {addr}` — move the range's lines to after the destination line.
            Ex::Move(_, dest) | Ex::Copy(_, dest) if !dest_in_range(ws, dest) => {
                status.error("E16: Invalid range");
            }
            Ex::Move(range, dest) => {
                let ((first, last), dest) = (ws.resolve_range(range), ws.resolve_addr(dest));
                let moved = dest.and_then(|dest| {
                    match run_ex(ExCmd::Move { first, last, dest }, ws, recorded) {
                        ExOutcome::Relocated(n) => n,
                        _ => None,
                    }
                });
                match moved {
                    // Moved onto itself (`:2m2`): nothing to report.
                    Some(n) => give_report(status, report::moved(n, ui_opts.report)),
                    None => status.error("E134: Cannot move a range of lines into itself"),
                }
            }
            // `:[range]t {addr}` / `:copy` — copy the range's lines to after the destination line.
            Ex::Copy(range, dest) => {
                let ((first, last), dest) = (ws.resolve_range(range), ws.resolve_addr(dest));
                let copied = dest.and_then(|dest| {
                    match run_ex(ExCmd::Copy { first, last, dest }, ws, recorded) {
                        ExOutcome::Relocated(n) => n,
                        _ => None,
                    }
                });
                match copied {
                    Some(n) => give_report(status, report::more(to_i64(n), ui_opts.report)),
                    None => status.error("E486: invalid copy"),
                }
            }
            // `:[range]>` / `:[range]<` — shift the range's lines one indent level per repeated verb char,
            // reusing the core `>>`/`<<` shift (no range = the current line).
            Ex::Shift {
                range,
                left,
                levels,
            } => {
                let (first, last) = ws.resolve_range(range);
                let ex = ExCmd::Shift {
                    first,
                    last,
                    left,
                    levels: levels.min(ruse_core::MAX_SHIFT_LEVELS),
                };
                let n = lines_outcome(&run_ex(ex, ws, recorded));
                give_report(
                    status,
                    report::shifted(
                        n,
                        left,
                        levels.min(ruse_core::MAX_SHIFT_LEVELS),
                        ui_opts.report,
                    ),
                );
                // A shift opens the fold the cursor ends in (nvim's `op_shift`), as `>>` does.
                if let Some(fv) = folds.get_mut(&ws.focused_buffer()) {
                    let pane = ws.focused();
                    crate::folds::open_at(
                        fv,
                        ruse_core::pos::line_of(pane.doc.bytes(), pane.view.cursor()),
                    );
                }
            }
            // `:[line]put [reg]` — put a register's text LINEWISE as new whole line(s) after the addressed
            // line (a charwise register is still put as whole lines — the linewise-forcing rule).
            Ex::Put { addr, reg, above } => {
                let put_addr = addr;
                // `:put!` puts above the line (#609).
                let addr = if above {
                    crate::app::ex_more::put_above(addr)
                } else {
                    addr
                };
                let target = {
                    let pane = ws.focused();
                    let b = pane.doc.bytes();
                    let cur = ruse_core::pos::line_of(b, pane.view.cursor());
                    put_addr.insert_index(cur, crate::app::dispatch::line_count(b))
                };
                let n = ws.resolve_addr(addr).map_or(0, |dest| {
                    lines_outcome(&run_ex(ExCmd::Put { dest, reg }, ws, recorded))
                });
                if n > 0 {
                    give_report(status, report::more(to_i64(n), ui_opts.report));
                } else if !matches!(reg, Some('+' | '*')) {
                    // An empty register: `E353`, with the cursor already on the addressed line (nvim's
                    // `ex_put` moves it there before it reads the register).
                    if let Some(line) = target {
                        goto_line(line.max(1), ws, files, recorded, status, quit);
                    }
                    status.error(format!("E353: Nothing in register {}", reg.unwrap_or('"')));
                } else {
                    status.clear();
                }
            }
            // `:[range]sort[!] [i][n][r][u] [/pattern/]` — sort the range's lines (whole file with no range).
            mut ex @ Ex::Sort(..) => {
                if let Err(msg) = crate::input::reuse_last_search(&mut ex, engine.last_search()) {
                    status.error(msg.to_string());
                    return;
                }
                let Ex::Sort(range, spec) = ex else {
                    unreachable!("bound by the arm's pattern")
                };
                let (first, last) = ws.resolve_range(range);
                let ex = ExCmd::Sort {
                    first,
                    last,
                    opts: spec,
                };
                let sorted = match run_ex(ex, ws, recorded) {
                    ExOutcome::Failed(e) => Err(e),
                    out => Ok(lines_outcome(&out)),
                };
                match sorted {
                    // nvim's `msgmore` for the lines `u` dropped; a sort that drops none says nothing.
                    Ok(removed) => {
                        give_report(status, report::more(-to_i64(removed), ui_opts.report))
                    }
                    // The pattern reader's own error, as `:sort` shows it (`E54`, no `E476` after it).
                    Err(e) => status.error(regex_error_msg(&e)),
                }
            }
            // F-007 multi-buffer navigation. `:enew` opens a scratch buffer and focuses it; `:ls` lists the
            // buffers; `:bn`/`:bp` cycle; `:b {n}`/`:b#` switch by number / to the alternate.
            Ex::Enew => {
                let id = ws.add_buffer(Vec::new(), None);
                ws.focus_buffer(id);
                status.echo(format!("[No Name] (buffer {})", id.0));
            }
            Ex::Buffers => status.echo(buffer_list_line(ws)),
            Ex::BufferNext => {
                ws.cycle_buffer(true);
                status.echo(focused_buffer_label(ws));
            }
            Ex::BufferPrev => {
                ws.cycle_buffer(false);
                status.echo(focused_buffer_label(ws));
            }
            Ex::Buffer(BufTarget::Alternate)
                if crate::app::ex_more::AltFile::current(alt_file, ws).is_some() =>
            {
                // The alternate file a write or rename named has no buffer yet: edit it (nvim's `:b#`
                // goes to the buffer it made for the name).
                let name = crate::app::ex_more::AltFile::current(alt_file, ws)
                    .unwrap_or_default()
                    .to_string();
                let spec = EditSpec {
                    bang: false,
                    file: Some(name),
                    cmd: None,
                    opts: crate::input::FileOpts::default(),
                };
                crate::app::file_cmds::ex_edit(&spec, ws, files, highlighters, status);
            }
            Ex::Buffer(target) => {
                let id = match target {
                    BufTarget::Number(n) => Some(DocumentId(n)),
                    BufTarget::Alternate => ws.alternate(),
                };
                match id {
                    Some(id) if ws.focus_buffer(id) => status.echo(focused_buffer_label(ws)),
                    Some(id) => status.error(format!("E86: buffer {} does not exist", id.0)),
                    None => status.error("E23: no alternate buffer"),
                }
            }
            // `:p`, `:l`, `:nu`, `:#` (#609): the range's lines in the message area, filtered by a `:filter`;
            // the cursor goes to the last line of the range at the wanted column, as `:N` does.
            Ex::Print { range, flags } => {
                let keep = match crate::app::ex_more::filter_keep(ws, ex_filter.as_ref()) {
                    Ok(keep) => keep,
                    Err(e) => {
                        status.error(regex_error_msg(&e));
                        return;
                    }
                };
                let (first, last) = crate::app::dispatch::resolve_normal_range(ws, range);
                let pane = ws.focused();
                let rows = crate::app::ex_print::rows(
                    pane.doc.bytes(),
                    first,
                    last,
                    flags,
                    pane.view.tabs(),
                    &*keep,
                );
                goto_line(last, ws, files, recorded, status, quit);
                crate::app::ex_more::show_rows(rows, status, overlay);
            }
            // `:z` (#609): a window of lines around the addressed one; the cursor goes to the line nvim's
            // `ex_z` picks, at column 0 when that is another line.
            Ex::Z(spec) => {
                let keep = match crate::app::ex_more::filter_keep(ws, ex_filter.as_ref()) {
                    Ok(keep) => keep,
                    Err(e) => {
                        status.error(regex_error_msg(&e));
                        return;
                    }
                };
                let (lnum, addressed) = match spec.line {
                    Some(r) => (crate::app::dispatch::resolve_normal_range(ws, r).1, true),
                    None => (
                        crate::app::dispatch::resolve_normal_range(
                            ws,
                            ruse_core::SubRange::CurrentLine,
                        )
                        .1,
                        false,
                    ),
                };
                let win_h = usize::from(rects.get(ws.focus()).map_or(text_rows, |r| r.h));
                // Twice `'scroll'` (half the window) with one window, the window less 3 with more, and the
                // screen less the command line under `!`.
                let window = if spec.bang {
                    usize::from(text_rows)
                } else if ws.window_count() == 1 {
                    win_h / 2 * 2
                } else {
                    win_h.saturating_sub(3)
                };
                let pane = ws.focused();
                let b = pane.doc.bytes();
                let nlines = crate::app::dispatch::line_count(b);
                let cur = ruse_core::pos::line_of(b, pane.view.cursor()) + 1;
                let (start, end, curs) =
                    crate::app::ex_print::z_window(&spec, lnum, addressed, window, nlines);
                let dashes = "-".repeat(usize::from(cols).saturating_sub(1));
                let mut rows = Vec::new();
                for n in start..=end {
                    let mark = spec.kind == Some('=') && n == lnum;
                    if mark {
                        rows.push(dashes.clone());
                    }
                    rows.extend(crate::app::ex_print::rows(
                        b,
                        n,
                        n,
                        spec.flags,
                        pane.view.tabs(),
                        &*keep,
                    ));
                    if mark {
                        rows.push(dashes.clone());
                    }
                }
                if curs != cur {
                    let at = crate::app::ex_more::line_start(b, curs);
                    place_cursor(ws, recorded, at);
                }
                crate::app::ex_more::show_rows(rows, status, overlay);
            }
            // `:=` (#609): the last line number of the range, or of the buffer.
            Ex::LineNumber(range) => {
                let n = match range {
                    Some(r) => crate::app::dispatch::resolve_normal_range(ws, r).1,
                    None => crate::app::dispatch::line_count(ws.focused().doc.bytes()),
                };
                status.info(n.to_string());
            }
            // `:retab` (#609).
            Ex::Retab {
                range,
                bang,
                tabstop,
            } => {
                let (first, last) = ws.resolve_range(range);
                let ex = ExCmd::Retab {
                    first,
                    last,
                    bang,
                    tabstop: tabstop.map(|v| *v),
                };
                run_ex(ex, ws, recorded);
            }
            // `:startinsert` and friends (#609) start Insert once the whole line has run.
            Ex::StartInsert { kind, bang } => *pending_insert = Some(Some((kind, bang))),
            Ex::StopInsert => *pending_insert = Some(None),
            // `:pwd` / `:cd` (#610). Buffer files keep pointing at the same files across a `:cd`.
            Ex::Pwd => match std::env::current_dir() {
                Ok(d) => status.info(d.display().to_string()),
                Err(e) => status.error(e.to_string()),
            },
            Ex::Cd(dir) => {
                let dir = match dir
                    .map(|d| if d == "-" { Ok(d) } else { expand(&d) })
                    .transpose()
                {
                    Ok(dir) => dir,
                    Err(e) => {
                        status.error(e.to_string());
                        return;
                    }
                };
                match crate::app::ex_more::cd_target(dir.as_deref(), prev_dir.as_deref()) {
                    Ok(target) => {
                        let shown = dir.as_deref().unwrap_or("~");
                        match crate::app::ex_more::change_dir(&target, shown, ws, files) {
                            Ok(old) => *prev_dir = Some(old),
                            Err(e) => status.error(e.to_string()),
                        }
                    }
                    Err(e) => status.error(e.to_string()),
                }
            }
            // `:file` (#610): show the file information, rename the buffer, or remove its name. The old name
            // becomes the alternate file.
            Ex::File(cmd) => {
                let id = ws.focused_buffer();
                let old = files.get(&id).map(|bf| bf.path.display().to_string());
                match cmd {
                    crate::input::FileCmd::Info => {
                        if let Some(msg) = info_status(&Command::FileInfo, ws) {
                            status.give(msg);
                        }
                    }
                    crate::input::FileCmd::Rename(name) => match expand(&name) {
                        Ok(name) => {
                            let fmt = files.get(&id).map(|bf| bf.fmt).unwrap_or_default();
                            crate::app::file_cmds::adopt_file(
                                ws,
                                files,
                                highlighters,
                                id,
                                &name,
                                fmt,
                            );
                            if let Some(old) = old.filter(|o| *o != name) {
                                *alt_file = Some(crate::app::ex_more::AltFile::new(ws, old));
                            }
                        }
                        Err(e) => status.error(e.to_string()),
                    },
                    crate::input::FileCmd::Clear => {
                        files.remove(&id);
                        highlighters.remove(&id);
                        ws.clear_focused_buffer_name();
                        if let Some(old) = old {
                            *alt_file = Some(crate::app::ex_more::AltFile::new(ws, old));
                        }
                    }
                }
            }
            // `:find` / `:sfind` (#610): edit the file found in `'path'` (`.,,`), in a new window for `:sf`.
            Ex::Find { split, spec } => {
                let name = spec.file.clone().unwrap_or_default();
                let current = files.get(&ws.focused_buffer()).map(|bf| bf.path.clone());
                match crate::app::ex_more::find_in_path(&name, current.as_deref()) {
                    Some(found) => {
                        if split {
                            ws.split(SplitDir::Horizontal);
                        }
                        let spec = EditSpec {
                            file: Some(found),
                            ..spec
                        };
                        if crate::app::file_cmds::ex_edit(&spec, ws, files, highlighters, status) {
                            then_run = spec
                                .cmd
                                .as_deref()
                                .map(crate::app::file_cmds::plus_cmd_line);
                        }
                    }
                    None => status.error(format!("E345: Can't find file \"{name}\" in path")),
                }
            }
            // `:drop {file}` (#610): go to a window that shows the file, or edit it here.
            Ex::Drop(spec) => {
                let name = spec.file.clone().unwrap_or_default();
                let shown =
                    crate::app::dispatch::buffer_for_path(files, std::path::Path::new(&name))
                        .and_then(|id| (0..ws.window_count()).find(|&i| ws.pane(i).doc.id() == id));
                let opened = match shown {
                    Some(i) => {
                        ws.focus_window(i);
                        true
                    }
                    None => crate::app::file_cmds::ex_edit(&spec, ws, files, highlighters, status),
                };
                if opened {
                    then_run = spec
                        .cmd
                        .as_deref()
                        .map(crate::app::file_cmds::plus_cmd_line);
                }
            }
            Ex::Fail(msg) => status.error(msg),
            Ex::Unknown(s) => status.error(format!("E492: Not an editor command: {s}")),
        }
        if let Some(grant) = trust_change {
            self.change_config_trust(grant);
        }
        if config_reload {
            self.reload_config(true);
        }
        if let Some(line) = then_run {
            let opened = self.status.take();
            self.execute_ex_one(line, frame);
            // The open's own report stays unless the command said something (an error, a count).
            if self.status.is_empty() {
                self.status.restore(opened);
            }
        }
    }

    /// Each window's display as its options and its scroll position are now (#615), for a frame that does
    /// not scroll (a key a `:normal` feeds).
    fn win_views(&self, rects: &[Rect]) -> Vec<WinView> {
        let has_signs = !self
            .lsp
            .diagnostics_for(self.ws.focused_buffer())
            .is_empty();
        rects
            .iter()
            .enumerate()
            .take(self.ws.window_count())
            .map(|(i, rect)| {
                let p = self.ws.pane(i);
                let mut ix = line_index::LineIndex::default();
                let idx = if p.view.doc() == self.ws.focused_buffer() {
                    &self.line_idx
                } else if let Some(idx) = self.pane_idx.get(p.view.doc(), p.doc.revision()) {
                    idx
                } else {
                    ix.refresh_for(p.view.doc(), p.doc.revision(), p.doc.bytes());
                    &ix
                };
                let text = PaneText {
                    bytes: p.doc.bytes(),
                    idx,
                };
                let folds = if i == self.ws.focus() {
                    self.folds
                        .get(&self.ws.focused_buffer())
                        .map_or(&[][..], Vec::as_slice)
                } else {
                    &[]
                };
                WinView::resolve(
                    p.view,
                    &self.ui_opts.display,
                    rect.w as usize,
                    rect.h as usize,
                    text.line_count(),
                    i == self.ws.focus() && has_signs,
                )
                .with_folds(folds)
            })
            .collect()
    }

    /// A frame for a key a `:normal` feeds: the buffer snapshot, line index and pane geometry as they are
    /// now (earlier keys of the same `:normal` may have edited, split or switched), with `frame`'s size.
    /// Insert mode opens the folds holding the cursor line (nvim's `edit` calls `foldOpenCursor`), also
    /// for keys run between frames (`:normal`, a `:g` of them).
    fn open_folds_in_insert(&mut self) {
        let pane = self.ws.focused();
        if !matches!(
            pane.view.mode(),
            Mode::Insert | Mode::Replace | Mode::VirtualReplace
        ) {
            return;
        }
        let line = ruse_core::pos::line_of(pane.doc.bytes(), pane.view.cursor());
        if let Some(fv) = self.folds.get_mut(&self.ws.focused_buffer()) {
            crate::folds::open_at(fv, line);
        }
    }

    fn nested_frame(&mut self, frame: &FrameCtx) -> FrameCtx {
        let (revision, snapshot) = {
            let f = self.ws.focused();
            (f.doc.revision(), f.doc.text_arc())
        };
        self.line_idx
            .refresh_for(self.ws.focused_buffer(), revision, &snapshot);
        let rects = window_rects(
            frame.cols,
            frame.text_rows,
            self.ws.window_count(),
            self.ws.split_dir(),
        );
        let wins = self.win_views(&rects);
        FrameCtx {
            revision,
            snapshot,
            rects,
            wins,
            cols: frame.cols,
            text_rows: frame.text_rows,
        }
    }

    /// Feed one key of a `:normal` through the whole session key path (#689), then run any replay it
    /// started (`@a`, `Q`, a linewise-Visual `@`) to completion, as Vim executes it inside the `:normal`.
    ///
    /// Returns whether a command failed: the rest of the replay it started is dropped then, and the caller
    /// drops the rest of its `:normal` keys too (#1085).
    fn nested_key(&mut self, key: KeyEvent, frame: &FrameCtx) -> bool {
        // CTRL-C stops a `:normal` (and the `:g` or range driving it) between keys.
        if ruse_core::interrupt::breakcheck() {
            self.macros.flush_replay();
            return true;
        }
        let f = self.nested_frame(frame);
        let status_before = self.status.serial();
        self.route(key, true, &f);
        self.open_folds_in_insert();
        if self.failed_since(status_before) {
            self.macros.flush_replay();
            return true;
        }
        while !self.quit {
            if let Some(line) = self.macros.next_line_run() {
                let b = self.ws.focused().doc.bytes();
                let lines =
                    ruse_core::pos::count_newlines(b) + usize::from(b.last() != Some(&b'\n'));
                let at = ruse_core::pos::nth_line_start(b, line.min(lines.saturating_sub(1))).0;
                place_cursor(&mut self.ws, &mut self.recorded, at);
            }
            if ruse_core::interrupt::breakcheck() {
                self.macros.flush_replay();
                return true;
            }
            let Some(k) = self.macros.pop_replay() else {
                break;
            };
            let f = self.nested_frame(frame);
            let status_before = self.status.serial();
            self.route_as(k, true, &f);
            self.open_folds_in_insert();
            if self.failed_since(status_before) {
                self.macros.flush_replay();
                return true;
            }
        }
        false
    }

    /// [`Session::route`] a key, its edits recorded under [`TransactionOrigin::Macro`] when it came from a
    /// macro replay's queue (`@{reg}`, `@:`, `Q`), else under the current origin (INV-ORIGIN, #880).
    fn route_as(&mut self, key: KeyEvent, from_macro: bool, frame: &FrameCtx) {
        let origin = self.ws.origin();
        if from_macro {
            self.ws.set_origin(TransactionOrigin::Macro);
        }
        self.route(key, from_macro, frame);
        self.ws.set_origin(origin);
    }

    /// Whether the key just routed failed (Vim beeps or gives an error), clearing the beep: nvim's
    /// `beep_flush`/`emsg` then flush the keys that were not typed (`flush_buffers(FLUSH_MINIMAL)`) — the rest
    /// of a macro replay, of a `:normal` payload, of a `:g` pass. Typed keys go on (#1085).
    fn failed_since(&mut self, status_before: u64) -> bool {
        let error = self.status.serial() != status_before && self.status.is_error();
        self.ws.take_beep() || error
    }

    /// One `:normal` pass of `events` at the cursor, then the implicit `<Esc>` that aborts an unfinished
    /// command and ends Insert (`:help :normal`). Every key takes the typed-key path, so the session's own
    /// resolution (`*`, `gd`, `H`/`M`/`L`, `&`, `@`, a nested `:` line, …) applies, which `engine.feed`
    /// alone skipped (#689).
    fn normal_pass(&mut self, events: &[KeyEvent], frame: &FrameCtx) {
        for &ev in events {
            if self.quit {
                return;
            }
            if self.nested_key(ev, frame) {
                break;
            }
        }
        if self.quit {
            return;
        }
        self.nested_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), frame);
        // The `:normal` saved and restores the keys around it, so its failure does not flush them.
        self.ws.take_beep();
        // The next line's column-0 placement relies on Normal (an Esc leaves Visual too, but not every
        // state a key can leave behind).
        if self.ws.focused().view.mode() != Mode::Normal {
            let Session {
                ws,
                files,
                recorded,
                status,
                quit,
                ..
            } = self;
            run_cmd(Command::EnterNormal, ws, files, recorded, status, quit);
        }
    }

    /// Run `body` as a `:normal`: refuse past [`MAX_NORMAL_DEPTH`] nested levels (E192), set aside the
    /// replay queue and an open `gQ` line (#807) so the keys reach Normal mode and the outer keys resume
    /// after, and record every change as one undo step (#852: Vim does not sync undo between keys that
    /// were not typed).
    fn as_normal(&mut self, kind: GroupKind, body: impl FnOnce(&mut Session)) {
        if self.normal_depth >= MAX_NORMAL_DEPTH {
            self.status.error("E192: Recursive use of :normal too deep");
            return;
        }
        self.normal_depth += 1;
        let insert_return = self.leave_insert_for_normal();
        let replay = self.macros.suspend_replay();
        let line = self.engine.suspend_cmdline();
        let doc = self.ws.focused_buffer();
        let opened = !self.ws.focused().doc.in_undo_block();
        if opened {
            self.ws.focused_doc_mut().begin_undo_block();
        }
        // The group a trace replays as one undo step, as this block makes it live (RFC-0020 R3).
        note_ex(ExCmd::Begin(kind), &mut self.recorded);
        body(self);
        note_ex(ExCmd::End, &mut self.recorded);
        if opened {
            if let Some(d) = self.ws.document_mut(doc) {
                d.end_undo_block();
            }
        }
        self.engine.resume_cmdline(line);
        self.macros.resume_replay(replay);
        if let Some(ret) = insert_return {
            self.resume_insert_after_normal(ret);
        }
        self.normal_depth -= 1;
    }

    /// A `:normal` typed from `i_CTRL-O` (the core mode still reads Insert) runs its keys in NORMAL mode,
    /// as Vim's `ex_normal` clears `restart_edit` (#1097): leave Insert with the caret where it was — on
    /// the last char when it sat past the end of the line, as `i_CTRL-O` does — and return where Insert
    /// resumes. `None` when not in Insert.
    fn leave_insert_for_normal(&mut self) -> Option<InsertReturn> {
        if self.ws.focused().view.mode() != Mode::Insert {
            return None;
        }
        let b = self.ws.focused().doc.bytes();
        let cur = self.ws.focused().view.cursor().0.min(b.len());
        let line = ruse_core::pos::line_of(b, ruse_core::BytePos(cur));
        let start = ruse_core::pos::nth_line_start(b, line).0.min(b.len());
        let at_eol = cur > start && b.get(cur).is_none_or(|&c| c == b'\n');
        let place = if at_eol { prev_char_start(b, cur) } else { cur };
        let Session {
            ws,
            files,
            recorded,
            status,
            quit,
            ..
        } = self;
        run_cmd(Command::EnterNormal, ws, files, recorded, status, quit);
        place_cursor(ws, recorded, place);
        Some(InsertReturn { line, at_eol })
    }

    /// Resume the Insert a [`Self::leave_insert_for_normal`] left: after the last char when the caret was
    /// past the end of the line and is still on that line's last char (Vim `edit()` with `ins_at_eol`),
    /// else at the caret. Nothing when the keys quit or left a mode other than Normal.
    fn resume_insert_after_normal(&mut self, ret: InsertReturn) {
        if self.quit || self.ws.focused().view.mode() != Mode::Normal {
            return;
        }
        let b = self.ws.focused().doc.bytes();
        let cur = self.ws.focused().view.cursor().0.min(b.len());
        let on_last = b.get(cur).is_some_and(|&c| c != b'\n')
            && b.get(next_char_start(b, cur)).is_none_or(|&c| c == b'\n');
        let cmd = if ret.at_eol
            && on_last
            && ruse_core::pos::line_of(b, ruse_core::BytePos(cur)) == ret.line
        {
            Command::EnterInsertAfter
        } else {
            Command::EnterInsert
        };
        let Session {
            ws,
            files,
            recorded,
            status,
            quit,
            ..
        } = self;
        run_cmd(cmd, ws, files, recorded, status, quit);
        self.engine.resume_insert_after_one_shot();
    }

    /// Execute `:[range]normal[!] {keys}` (`:help :normal`). With no range it runs ONCE at the cursor
    /// (column kept); with a range it runs once per line, cursor at column 0. As in Vim (nvim v0.12.4), the
    /// loop runs over the ORIGINAL `[line1, line2]` and each step targets `min(lnum, last_line)` of the
    /// CURRENT buffer, so `:%normal dd` deletes every line. `bang` (no user mappings) is inert: ruse has
    /// none yet.
    fn ex_normal(&mut self, range: Option<ruse_core::SubRange>, keys: &str, frame: &FrameCtx) {
        let events = crate::keys::parse_normal_keys(keys);
        self.as_normal(GroupKind::Normal, |s| match range {
            None => s.normal_pass(&events, frame),
            Some(range) => {
                let (line1, line2) = crate::app::dispatch::resolve_normal_range(&s.ws, range);
                for lnum in line1..=line2 {
                    if s.quit || ruse_core::interrupt::interrupted() {
                        break;
                    }
                    let b = s.ws.focused().doc.bytes();
                    let last = crate::app::dispatch::line_count(b).max(1);
                    let off = ruse_core::pos::nth_line_start(b, lnum.min(last) - 1).0;
                    place_cursor(&mut s.ws, &mut s.recorded, off);
                    s.normal_pass(&events, frame);
                }
            }
        });
    }

    /// Execute `:[range]g/pat/normal {keys}` (`:help :g`) over the lines PASS 1 marked: `{keys}` run as a
    /// `:normal` at column 0 of each marked line that still exists ([`Self::over_global_marks`]).
    fn ex_global_normal(&mut self, marks: &[usize], keys: &str, frame: &FrameCtx) {
        let events = crate::keys::parse_normal_keys(keys);
        self.over_global_marks(marks, |s| {
            s.normal_pass(&events, frame);
            true
        });
        self.status.info(format!("{} lines changed", marks.len()));
    }

    /// Execute `:[range]g/pat/{cmd}` for any command but the ones core runs in one pass (#1245): `cmd`, `|`
    /// and all, runs through the Ex dispatch once per marked line that still exists, with the cursor at
    /// the start of that line and so the command's default range there (Vim's `global_exe`). The first
    /// command that fails ends the `:g` (Vim's `emsg` bumps `global_busy`); one a `:silent!` hid does not.
    /// The whole `:g` is one undo step, and the trace records it as a group of the commands it ran.
    fn ex_global_ex(&mut self, marks: &[usize], cmd: &str, frame: &FrameCtx) {
        let pieces = crate::input::split_bar(cmd);
        let lines = |s: &Session| crate::app::dispatch::line_count(s.ws.focused().doc.bytes());
        let before = lines(self);
        let mut last = None;
        self.over_global_marks(marks, |s| {
            s.run_ex_pieces(pieces.clone(), frame);
            if !s.status.is_empty() {
                last = Some(s.status.shown());
            }
            !(s.status.is_error() || s.swap.is_some() || s.confirm.is_some())
        });
        if self.status.is_error() || self.swap.is_some() || self.confirm.is_some() {
            return;
        }
        // Vim reports the lines the commands added or removed (`msgmore`, past 'report' = 2), else the
        // message of the last command that left one.
        let after = lines(self);
        // The commands ran with their reports kept out of the history (as nvim's `global_busy` holds back
        // `msgmore`); the one report the `:g` gives is kept.
        match report::line_delta(before, after, self.ui_opts.report) {
            Some(msg) => self.status.info(msg),
            None => match last {
                Some(msg) => self.status.give(msg),
                None => self.status.clear(),
            },
        }
    }

    /// PASS 2 of a `:g` (`:help :g`): run `each` once per line PASS 1 marked (`marks`, 1-based) that still
    /// exists, the cursor placed at its start, until `each` returns `false`. The marks are anchors that move
    /// with the edits (#853, INV-ANCHOR), so a mark follows its line wherever earlier passes moved it, and a
    /// line deleted (or joined into another) is skipped, as Vim's line marks are. Lines the commands insert
    /// are never marked. The pass runs as a `:normal` does (one undo step, a trace group) with
    /// [`Session::global_busy`] set.
    fn over_global_marks(&mut self, marks: &[usize], mut each: impl FnMut(&mut Session) -> bool) {
        use ruse_core::{AnchorPolicy, Bias, BytePos};
        let doc_id = self.ws.focused_buffer();
        // Each line is held by two anchors: its start (pushed right by text inserted before it) and the
        // start of the next line (held left). A deleted line collapses them together.
        let anchors: Vec<_> = {
            let doc = self.ws.focused_doc_mut();
            // Every line start in one pass: a start per mark looked up by a scan from the top made a `:g`
            // over many lines quadratic before it ran a single pass.
            let b = doc.bytes();
            let starts: Vec<usize> = std::iter::once(0)
                .chain(newline_offsets(b).map(|i| i + 1))
                .collect();
            let start = |l: usize| starts.get(l).copied().unwrap_or(b.len());
            let spans: Vec<(usize, usize)> =
                marks.iter().map(|&l| (start(l - 1), start(l))).collect();
            // One index rebuild for all of them, not one per anchor.
            let ids = doc.create_anchors(spans.iter().flat_map(|&(s, e)| {
                [
                    (BytePos(s), Bias::After, AnchorPolicy::Clamp),
                    (BytePos(e), Bias::Before, AnchorPolicy::Clamp),
                ]
            }));
            spans
                .iter()
                .zip(ids.as_chunks::<2>().0)
                .map(|(&(s, e), &[a, z])| (a, z, s == e))
                .collect()
        };
        let busy = std::mem::replace(&mut self.global_busy, true);
        // The marks already visited are dropped as the pass goes (#1257). A deleted line's two anchors
        // collapse where it was, which is where the next command edits, so kept they would all be carried
        // through every later edit. Each removal re-sorts the store at the next edit, so they go in
        // batches of about the square root of the anchor count.
        let batch = anchors.len().isqrt().max(32) * 2;
        let mut visited: Vec<ruse_core::AnchorId> = Vec::with_capacity(batch);
        // The commands' own reports stay out of `:messages`; the `:g` gives its one report after (nvim's
        // `global_busy` holds back `msgmore`). Errors are kept.
        let hushed = self.status.hush(Silence::Messages);
        self.as_normal(GroupKind::Global, |s| {
            for &(start, end, empty_buffer) in &anchors {
                if s.quit || s.ws.focused_buffer() != doc_id || ruse_core::interrupt::interrupted()
                {
                    break;
                }
                if visited.len() >= batch {
                    s.ws.focused_doc_mut().remove_anchors(visited.drain(..));
                }
                visited.extend([start, end]);
                let doc = s.ws.focused().doc;
                let (Some(a), Some(z)) = (doc.resolve_anchor(start), doc.resolve_anchor(end))
                else {
                    continue;
                };
                let (a, z) = (a.offset.0, z.offset.0);
                let b = doc.bytes();
                let at_line_start = a == 0 || b.get(a - 1) == Some(&b'\n');
                if !(a < z || empty_buffer) || !at_line_start {
                    continue; // the line was deleted, or joined into the one before it
                }
                place_cursor(&mut s.ws, &mut s.recorded, a);
                if !each(s) {
                    break;
                }
            }
        });
        self.global_busy = busy;
        self.status.unhush(hushed);
        if let Some(doc) = self.ws.document_mut(doc_id) {
            doc.remove_anchors(anchors.into_iter().flat_map(|(start, end, _)| [start, end]));
        }
    }

    /// Test seam: run ONE Ex command (`line`, no leading `:`) against `ws` through the session's Ex
    /// dispatch, as a typed `:{line}<CR>` would after the command line closes. `ws` is moved into a fresh
    /// session for the call and back out after. Returns the session for its status / quit / confirm.
    #[cfg(test)]
    pub(crate) fn ex_on(ws: &mut Workspace, line: &str) -> Session {
        let mut s = Session::new(SessionInit {
            path: None,
            fmt: persist::encoding::FileFormat::default(),
            initial: ws.focused().doc.bytes().to_vec(),
            status: String::new(),
            profile: "vim",
            term: TermInfo::default(),
            cwd: PathBuf::new(),
            trusted: false,
            rescue: false,
        });
        std::mem::swap(&mut s.ws, ws);
        let frame = s.pre_frame(80, 24);
        s.execute_ex_one(line.to_string(), &frame);
        std::mem::swap(&mut s.ws, ws);
        s
    }
}

/// `:earlier {n}` (`older`) / `:later {n}`: `n` steps of `g-` / `g+`, recorded and applied one by one so
/// each lands in the trace. The walk stops at the end of the undo time line, where a step changes nothing,
/// so a huge count costs no more steps than there are changes (#685); CTRL-C stops it too.
fn time_travel(
    older: bool,
    n: u32,
    ws: &mut Workspace,
    files: &Files,
    recorded: &mut Vec<Command>,
    status: &mut Status,
    quit: &mut bool,
) {
    let cmd = if older {
        Command::UndoOlder
    } else {
        Command::UndoNewer
    };
    for _ in 0..n {
        let before = ws.focused().doc.revision();
        run_cmd(cmd.clone(), ws, files, recorded, status, quit);
        if ws.focused().doc.revision() == before || ruse_core::interrupt::breakcheck() {
            break;
        }
    }
    let dir = if older { "earlier" } else { "later" };
    status.info(format!("{n} change(s) {dir}"));
}

/// The offsets of every `\n` in `b`, in order.
fn newline_offsets(b: &[u8]) -> impl Iterator<Item = usize> + '_ {
    b.iter()
        .enumerate()
        .filter(|&(_, &c)| c == b'\n')
        .map(|(i, _)| i)
}

/// Whether an Ex command's status reports a failure that ends a `|`-chained line (`:help :bar`): a Vim
/// error (`E{n}: …`, an unknown command's `E492` among them).
/// Whether an Ex command starts another program, a risky operation the journal syncs before (RFC-0023
/// §4): `:!`, a filter, `:r !`, `:w !` and `:terminal`.
fn runs_external(ex: &Ex) -> bool {
    matches!(
        ex,
        Ex::Shell(_)
            | Ex::Filter { .. }
            | Ex::Terminal
            | Ex::Read {
                source: ReadSource::Command(_),
                ..
            }
    ) || matches!(ex, Ex::Write(spec) if matches!(spec.target, WriteTarget::Command(_)))
}

/// The journal part of `:checkhealth` (RFC-0023 §8): the journal directory and each file buffer's journal,
/// in buffer order.
fn journal_health(
    rescue: bool,
    journals: Option<&persist::journal::Journals>,
    files: &Files,
    ws: &Workspace,
    writers: &HashMap<DocumentId, persist::journal::Writer>,
) -> health::Journaling {
    if !rescue {
        return health::Journaling::Off;
    }
    let Some(j) = journals else {
        return health::Journaling::NoStateDir;
    };
    let now = std::time::Instant::now();
    let mut ids: Vec<DocumentId> = files.keys().copied().collect();
    ids.sort_by_key(|id| id.0);
    let buffers = ids
        .into_iter()
        .filter_map(|id| {
            let bf = files.get(&id)?;
            Some(health::JournalInput {
                file: bf.path.display().to_string(),
                modified: ws.document(id).is_some_and(|d| d.is_modified()),
                health: writers.get(&id).map(|w| w.health(now)),
            })
        })
        .collect();
    health::Journaling::On {
        dir: j.dir().display().to_string(),
        archive: persist::retention::stats(j),
        buffers,
    }
}

/// [`Session::sync_journals`] over the session's fields, for a caller that holds them borrowed apart.
fn sync_journals(
    ws: &mut Workspace,
    files: &Files,
    journal_writers: &mut HashMap<DocumentId, persist::journal::Writer>,
    journal_failed: &mut HashSet<DocumentId>,
    status: &mut Status,
) {
    let now = std::time::Instant::now();
    for (id, w) in journal_writers.iter_mut() {
        let (Some(doc), Some(bf)) = (ws.document_mut(*id), files.get(id)) else {
            continue;
        };
        w.absorb(doc, bf.fmt, now); // the changes made since the frame began, too
        if w.unsynced() {
            let r = w.flush(doc);
            journal_outcome(r, *id, &bf.path, w.journal(), journal_failed, status);
        }
    }
}

/// Report a journal sync: a failure is shown once per failing run, as Neovim's E303 is, rather than
/// silently leaving the work unprotected (#907); the records stay buffered for the next try.
/// The status line then shows `[journal!]` until a write succeeds (RFC-0023 §8).
fn journal_outcome(
    r: io::Result<()>,
    id: DocumentId,
    path: &std::path::Path,
    journal: &std::path::Path,
    failed: &mut HashSet<DocumentId>,
    status: &mut Status,
) {
    match r {
        Ok(()) => {
            failed.remove(&id);
        }
        Err(e) => {
            if failed.insert(id) {
                crate::log::journal_failed(path, &e);
                status.error(format!(
                    "E303: unable to write the recovery journal for \"{}\": {e} ({})",
                    path.display(),
                    journal.display()
                ));
            }
        }
    }
}

/// `read_only_mode` is `ruse -R` (#1211).
pub(crate) fn run(
    path: Option<PathBuf>,
    raw: Vec<u8>,
    read_only_mode: bool,
    safe_mode: bool,
) -> io::Result<()> {
    // F-008: detect the original encoding/line-ending once, edit in clean LF, restore it on save.
    let fmt = persist::encoding::FileFormat::detect(&raw);
    let disk = fmt.to_buffer(&raw);
    // A crash may have left a journal of unsaved work; offer it, never auto-apply it. A journal that exists
    // but cannot be read or replayed is reported on the status line, never silently skipped (#897). The
    // owner (and the unnamed buffer's journal name) is fixed now, at startup (#1192).
    let _ = persist::journal::owner();
    // RFC-0023 §9 (#934): at most once a day, old archives go; live journals are never touched.
    if let Some(j) = persist::journal::current() {
        let _ = persist::retention::sweep_if_due(j, persist::journal::now_ms());
    }
    let offer = persist::journal::current()
        .map_or(persist::Recovery::None, |j| j.offer(path.as_deref(), &disk));

    // hlsearch/incsearch match spans, cached on (revision, viewport, pattern) like the syntax highlighter
    // — no full-buffer regex per frame (F-009 #1).
    let mut cached_search = highlight::CachedSearch::default();

    let guard = TermGuard::enter()?;
    let mut out = io::stdout();
    // Create crossterm's event source now, so its SIGWINCH registration exists before a fatal signal can
    // arrive: the signal handler wakes a blocked read through it (#857). Installed before the recovery
    // prompt, so a signal there also unwinds through the guard (#897).
    let _ = event::poll(std::time::Duration::ZERO);
    crate::signals::install();

    // Open-time crash recovery (F-008 #3/#4, RFC-0023 §7): the user picks; the original file is never
    // touched. The buffer always starts as the disk text, which is also the trace's starting text; a
    // recovery is applied over it once the session exists, as an edit (#863).
    let shown = path.as_ref().map_or_else(
        || "the unnamed buffer".to_string(),
        |p| p.display().to_string(),
    );
    let (recovered, read_only, status) = match crate::ui::recovery::resolve(
        &mut out,
        offer,
        &shown,
        path.as_deref(),
        &disk,
        fmt,
        "quit",
    )? {
        crate::ui::recovery::Resolved::Open {
            recovered,
            read_only,
            note,
        } => (
            recovered,
            read_only,
            note.map_or_else(
                || String::from("ruse — :q to quit"),
                |n| format!("ruse — {n}"),
            ),
        ),
        // The journal stays for the next start.
        crate::ui::recovery::Resolved::Quit { .. } => return Ok(()),
    };
    let initial = disk;

    // The config layers (#614): machine, user, and the workspace's `.ruse/config.toml` when its trust record
    // matches. Loading never fails; the problems are diagnostics, shown once on the status line.
    let cwd = std::env::current_dir().unwrap_or_default();
    let config_paths = crate::config::ConfigPaths::from_env(&cwd);
    // Safe mode (CFG-SAFE-001): asked for, or after a crash loop the boot record counted. A start that
    // reads user config counts itself until its first frame clears the record (`config::boot`).
    let boot_record = crate::config::boot::BootRecord::from_env();
    let safe = crate::config::boot::decide(
        safe_mode,
        boot_record
            .as_ref()
            .map_or(0, crate::config::boot::BootRecord::failed),
    );
    let config = crate::config::load_mode(&config_paths, safe);
    // Safe mode is a degrade: the user and workspace layers are not read (#966).
    if let Some(mode) = safe {
        crate::log::degrade(
            "config",
            match mode {
                crate::config::SafeMode::Asked => "safe_mode_asked",
                crate::config::SafeMode::CrashLoop { .. } => "safe_mode_crash_loop",
            },
            "",
        );
    }
    let mut boot_armed = crate::config::boot::arm(
        boot_record.clone(),
        safe,
        crate::config::reads_user_layers(&config),
    );
    for d in &config.diagnostics {
        crate::log::config_diagnostic(d.code);
    }
    let status = match crate::config::startup_notice(&config) {
        Some(note) if status == "ruse — :q to quit" => note,
        Some(note) => format!("{status} · {note}"),
        None => status,
    };
    // Profile selection (F-012 / F-013): `RUSE_PROFILE` for this run, else the config's `input.profile`.
    // `emacs` = Emacs, `native` = Native, else Vim.
    let configured_profile = config.get("input.profile").and_then(|e| match &e.value {
        crate::config::Value::Str(s) => Some(s.clone()),
        _ => None,
    });
    let profile = match std::env::var("RUSE_PROFILE")
        .ok()
        .or(configured_profile)
        .as_deref()
    {
        Some("emacs") => "emacs",
        Some("native") => "native",
        _ => "vim",
    };
    // INV-TRUST-1 / D-058: language servers execute workspace code (build scripts, proc-macros), so they
    // start only once the user trusts the workspace: the user config's `workspace.trusted_roots` or
    // `RUSE_TRUSTED_WORKSPACES` at startup, or `:trust`.
    let configured_roots: Vec<PathBuf> = match config.get("workspace.trusted_roots") {
        Some(crate::config::Effective {
            value: crate::config::Value::List(roots),
            ..
        }) => roots.iter().map(PathBuf::from).collect(),
        _ => Vec::new(),
    };
    let trusted =
        crate::trust::workspace_trusted(&cwd) || crate::trust::is_trusted(&cwd, configured_roots);
    let mut s = Session::new(SessionInit {
        path,
        fmt,
        initial,
        status,
        profile,
        term: TermInfo {
            sgr_mouse: guard.sgr_mouse(),
            sync_output: guard.sync_output(),
            bracketed_paste: guard.bracketed_paste(),
            focus_events: guard.focus_events(),
        },
        cwd,
        trusted,
        rescue: true,
    });
    // What the start buffer's file held as ruse read it, for `[disk changed]` (RFC-0023 §8).
    if let Some(p) = s
        .files
        .get(&s.ws.focused_buffer())
        .map(|bf| bf.path.clone())
    {
        persist::disk::saw(&p, &raw);
    }
    s.config = config;
    s.config_stamp = crate::config::Stamp::of(&config_paths);
    s.config_paths = Some(config_paths);
    s.boot_record = boot_record;
    s.apply_config(true);
    if let Some(rec) = recovered {
        s.apply_recovered(rec);
    }
    if read_only {
        for bf in s.files.values_mut() {
            bf.read_only = true; // the start buffer is the only one with a file yet
        }
    }
    s.readonly.mode = read_only_mode;
    // `"+`/`"*` (`:help quoteplus`): inject the real OS-clipboard provider (shell-out to pbcopy/wl-copy/
    // xclip/clip, resolved once). Degrades to a no-op when no clipboard tool is on PATH, so a headless run
    // still works — `"+p` just pastes nothing.
    s.ws.set_clipboard(Box::new(crate::clipboard::SystemClipboard::detect()));
    // CTRL-C (`C-g` in Emacs) interrupts a long command: the core's checkpoints poll the terminal (#898).
    crate::breakcheck::install(profile == "emacs");

    // The previous frame's cell grid — the render diff emits only what changes against it (F-006).
    // Starts empty so the first frame paints in full.
    let mut prev_frame = screen::Screen::new(0, 0);
    let sync_output = guard.sync_output(); // pinned once from the F-010 ledger (INV-RENDER-PROFILE)
                                           // F-031 slice 3b-2b: the pinned inline-graphics protocol + the persistent placement state the graphics
                                           // pass reconciles each frame. A graphics-capable terminal reserves taller image blocks (room for pixels).
                                           // Only KITTY is lowered in this slice; a Sixel/iTerm2 terminal falls to the placeholder (its escapes
                                           // are 3b-3), so we must NOT emit Kitty bytes there. `graphics.rs` speaks Kitty only.
    let has_graphics = guard.graphics() == crate::caps::ledger::GraphicsProtocol::Kitty;
    let image_rows: u16 = if has_graphics { 12 } else { 2 };
    // Inside tmux the graphics escapes must be wrapped so tmux forwards them (needs `allow-passthrough on`).
    let in_tmux = std::env::var_os("TMUX").is_some();
    let mut resident: HashSet<graphics::ImageId> = HashSet::new();
    let mut last_paint = std::time::Instant::now();
    // A start that crashes after loading the config and before its first frame, on demand: the crash
    // loop safe mode detects, for the end-to-end test (`tests/pty_config_safe.rs`).
    if std::env::var_os("RUSE_TEST_CRASH_BEFORE_FIRST_FRAME").is_some() {
        std::process::abort();
    }

    while !s.quit {
        // SIGTERM / SIGHUP (#857): end the session; the caller journals every modified buffer and the
        // guard restores the terminal. The handler's wake-up lands here via a resize event.
        if crate::signals::pending().is_some() {
            break;
        }
        let (cols, term_rows) = ct_terminal::size().unwrap_or((80, 24));
        s.idle_disk_check();
        let frame = s.pre_frame(cols, term_rows);
        // A macro replay repaints at most every `REPLAY_PAINT`, not once per replayed key: nvim does not
        // redraw while it executes typeahead, and a frame per key made a 100,000-key replay take half a
        // minute (#787). A typed key is always painted.
        let paint = !s.replaying() || last_paint.elapsed() >= REPLAY_PAINT;
        if paint {
            last_paint = std::time::Instant::now();
            let (revision, snapshot, rects) = (frame.revision, &frame.snapshot, &frame.rects);
            // Highlight only the VISIBLE byte range of the focused buffer (F-015 #3): the union of the
            // viewports of every pane showing it. The tree is reparsed incrementally on edit (keyed on
            // revision); the viewport-bounded query keeps the per-keystroke cost O(viewport), not O(buffer).
            let visible = visible_byte_range(&s.ws, rects, &s.line_idx, snapshot.len());
            // Syntax highlighting is the FOCUSED buffer's own grammar (F-007): each file buffer has its own
            // highlighter in the registry; a scratch/no-file buffer has none, so it paints unhighlighted.
            let focused_id = s.ws.focused_buffer();
            // F-031 3b-2b: reserve taller image blocks on a graphics-capable terminal (no-op after the first).
            if let Some(h) = s.highlighters.get_mut(&focused_id) {
                h.set_image_rows(image_rows);
            }
            let flags = s.status_flags();
            let (spans, virt_lines): (&[highlight::Span], &[highlight::VirtLine]) =
                match s.highlighters.get_mut(&focused_id) {
                    Some(h) => h.spans_and_virt(revision, snapshot, visible.clone()),
                    None => (&[], &[]),
                };
            // The focused pane's extra reverse-video highlights: a `:s///c` confirm match, else the
            // incsearch pattern being typed in `/`…`?`, else the last search (hlsearch) — F-009 #1. The
            // search matches are viewport-cached (CachedSearch), not a per-frame full-buffer regex.
            let focus_hl: Vec<(usize, usize)> = if let Some(c) = &s.confirm {
                c.subs
                    .get(c.idx)
                    .map(|s| vec![(s.start.0, s.end.0)])
                    .unwrap_or_default()
            } else {
                let active = match s.engine.cmdline() {
                    // Incsearch: the pattern being typed, only when `incsearch` is on.
                    Some(('/', buf, _)) | Some(('?', buf, _))
                        if s.incsearch_on && !buf.is_empty() =>
                    {
                        Some(buf.to_string())
                    }
                    // Hlsearch: the last completed search, only when `hlsearch` is on.
                    _ if s.hlsearch_on => s.search_hl.clone(),
                    _ => None,
                };
                active
                    .map(|p| {
                        cached_search
                            .spans_in(
                                s.ws.focused_buffer(),
                                revision,
                                snapshot,
                                visible.clone(),
                                &p,
                            )
                            .to_vec()
                    })
                    .unwrap_or_default()
            };
            // Overlay match rows painted above the status line — at most one picker is open at a time, so all
            // share the same uniform `rows()` paint slot (F-004 / F-013 NAT-3).
            let overlay_rows: Vec<(String, bool)> = if let Some(o) = s.overlay.as_ref() {
                o.rows()
            } else if s.engine.cmdwin().is_some() {
                // The command-line window (`q:`/`q/`/`q?`) lists the history ring in the same overlay slot.
                s.engine.cmdwin_rows()
            } else {
                // F-014: an LSP hover result shares the overlay slot (no picker can be open here).
                s.lsp.hover_overlay().unwrap_or_default()
            };
            // The Native leader (which-key) hint owns the command line while armed (F-013 NAT-2), shown with a
            // Space prefix — below the overlay, above the ordinary `:`/`/` line (none can co-occur).
            let leader_hint = s.engine.leader_hint();
            // The engine's command line also places the terminal caret (F-026 line editor); a picker's caret
            // stays at the end of its query.
            let mut cmd_caret = None;
            let cmd_line: Option<(char, &str)> = if let Some(o) = s.overlay.as_ref() {
                Some(o.prompt())
            } else if let Some(h) = leader_hint.as_ref() {
                Some((' ', h.as_str()))
            } else if let Some(kind) = s.engine.cmdwin() {
                // The command-line window's prompt glyph (`:`/`/`/`?`) with a short hint that `<CR>` runs the line.
                Some((kind, "[cmdwin] <CR> run  <Esc> close"))
            } else {
                s.engine.cmdline().map(|(pfx, t, caret)| {
                    cmd_caret = Some(caret);
                    (pfx, t)
                })
            };
            // F-011: reap terminals whose buffer was closed (Drop hangs up the child), resize each to its window
            // rect (so the child reflows), then pull pending PTY output into its VT grid so it paints this frame.
            // `term_views` lends each grid to the renderer. On non-unix this is always empty.
            #[cfg(unix)]
            let term_views: crate::ui::render::TermViews = {
                let live: std::collections::HashSet<DocumentId> =
                    s.ws.buffers().iter().map(|b| b.id).collect();
                s.terminals.retain(|id, _| live.contains(id));
                for (i, rect) in rects.iter().enumerate().take(s.ws.window_count()) {
                    let doc = s.ws.pane(i).view.doc();
                    if let Some(t) = s.terminals.get_mut(&doc) {
                        t.resize(rect.h.max(1), rect.w.max(1));
                    }
                }
                for t in s.terminals.values_mut() {
                    t.drain();
                }
                s.terminals.iter().map(|(id, t)| (*id, t.grid())).collect()
            };
            #[cfg(not(unix))]
            let term_views = crate::ui::render::TermViews::new();
            // F-014: sync the focused buffer with its server + poll all clients (diagnostics + response
            // dispatch) — the coordinator owns all of it; deferred edits/refs are applied after render.
            s.lsp
                .sync_and_poll(&s.ws, &s.files, revision, snapshot, &mut s.status);
            let focus_diags = s.lsp.diagnostics_for(s.ws.focused_buffer());
            // F-031 3b-2c: read each visible image's pixel dimensions (IHDR only) so render can size it to its
            // natural aspect and centre it. Cheap — only the 24-byte header, only when graphics are on.
            let image_dims = read_image_dims(has_graphics, virt_lines);
            // A mid-session recovery prompt (#859) cleared the terminal: repaint every cell this frame.
            if crate::ui::prompts::take_screen_clobbered() {
                prev_frame = screen::Screen::new(0, 0);
            }
            // The wildmenu and `c_CTRL-D` listing belong to the engine's own `:` line (#613).
            let cmd_menu = if cmd_caret.is_some() {
                crate::ui::render::CmdMenu {
                    pum: s.engine.cmdline_wild(),
                    listing: s.engine.cmdline_listing(),
                }
            } else {
                crate::ui::render::CmdMenu::default()
            };
            let images = render(
                &mut out,
                &s.ws,
                cmd_line.map(|(pfx, t)| (pfx, t, cmd_caret)),
                &cmd_menu,
                s.status.text(),
                spans,
                virt_lines,
                rects,
                &frame.wins,
                &mut prev_frame,
                sync_output,
                &focus_hl,
                &overlay_rows,
                &term_views,
                focus_diags,
                s.lsp.completion_view(),
                has_graphics,
                &image_dims,
                s.folds
                    .get(&s.ws.focused_buffer())
                    .map_or(&[][..], Vec::as_slice),
                &s.line_idx,
                &s.pane_idx,
                s.ui_opts.showmode,
                s.ui_opts.conceal,
                &flags,
                s.status.serial(),
                &mut s.pager,
            )?;
            // F-031 slice 3b-2b: the graphics pass — after the cell flush, draw real pixels for the focused
            // pane's image blocks on a graphics-capable terminal (else the placeholder painted above stands).
            if has_graphics {
                let read_png = |path: &str| -> Option<Vec<u8>> {
                    let meta = std::fs::metadata(path).ok()?;
                    if meta.len() > (16 << 20) {
                        return None; // coarse decode-bomb / runaway guard (16 MiB)
                    }
                    let bytes = std::fs::read(path).ok()?;
                    graphics::png_dimensions(&bytes).is_some().then_some(bytes)
                };
                graphics::graphics_pass(&mut out, &images, &mut resident, in_tmux, read_png)?;
            }
        }
        s.post_render(&frame);
        // The first frame is drawn: the config booted, so this start is no crash (`config::boot`).
        if let Some(b) = boot_armed.take() {
            b.clear();
        }
        #[cfg(debug_assertions)]
        crate::log::debug_panic_if_requested();
        // Keys the session queued come before the terminal: the key half of a split Alt chord (#896), then
        // macro replay (D-055), so `@{reg}`'s decoded keys flow through the same dispatch as if re-typed. A
        // key from the replay queue is `from_replay`.
        let (raw_key, from_replay) = match s.next_queued() {
            Some(queued) => queued,
            None => {
                // Input read ahead while a long command polled for CTRL-C comes first, in order.
                let ev = if let Some(ev) = crate::breakcheck::pop() {
                    ev
                } else if let Some(ms) = s.poll_ms() {
                    if event::poll(std::time::Duration::from_millis(ms))? {
                        event::read()?
                    } else {
                        continue; // timed out: re-render freshly-drained async output
                    }
                } else {
                    event::read()?
                };
                match ev {
                    Event::Key(k) => (k, false),
                    // A bracketed paste (mode 2004) arrives as one event and never as keys (#600).
                    Event::Paste(text) => {
                        s.on_paste(&text, &frame);
                        continue;
                    }
                    // Focus gain looks at the files on disk again (RFC-0023 §8), as nvim's `:checktime`
                    // on FocusGained. The guard asks for focus reports (mode 1004, #1220) where the
                    // capability ledger has them.
                    Event::FocusGained => {
                        s.check_disk(true);
                        continue;
                    }
                    _ => continue,
                }
            }
        };
        if raw_key.kind != KeyEventKind::Release && s.answer_prompt(raw_key) {
            continue;
        }
        s.on_key(raw_key, from_replay, &frame);
    }
    // A normal quit lets every buffer's unsaved changes go (#862): `:q` only gets here with none, and
    // `:q!` / `:qa!` / `:cq` discard them on purpose. nvim deletes its swap files on each of these exits, so
    // the next start offers nothing. A signal ends the loop without `quit`, and its journals are kept.
    if s.quit && crate::signals::pending().is_none() {
        s.discard_journals();
    }
    // F-031 3b-2b: clear any inline images on exit so none is left drawn after the editor quits.
    if has_graphics {
        use std::io::Write;
        let del = graphics::delete_all();
        let del = if in_tmux {
            graphics::wrap_tmux(&del)
        } else {
            del
        };
        let _ = out.write_all(&del);
        let _ = out.flush();
    }
    Ok(())
}

/// For `*`/`#`/`g*`/`g#` ([`Command::SearchWordUnder`]): record the keyword pattern the planner will search
/// for as the engine's last search (with its direction) and as the hlsearch pattern. `false` (with E348 on
/// the status line) when the line has no keyword, so the caller runs nothing. Any other command passes.
fn note_word_search(
    cmd: &Command,
    ws: &Workspace,
    engine: &mut InputEngine,
    search_hl: &mut Option<String>,
    status: &mut Status,
) -> bool {
    let Command::SearchWordUnder {
        forward,
        whole_word,
        ..
    } = *cmd
    else {
        return true;
    };
    match ws.word_search_pattern(whole_word) {
        Some(pat) => {
            engine.set_last_search(pat.clone(), forward);
            *search_hl = Some(pat);
            true
        }
        None => {
            status.error("E348: No string under cursor");
            false
        }
    }
}

const E35: &str = "E35: No previous regular expression";
/// `zf` or `:fold` while 'foldmethod' makes the folds (nvim's `e_cant_create_fold_with_current_foldmethod`).
const E350: &str = "E350: Cannot create fold with current 'foldmethod'";

/// Whether `cmd` is a search the engine issued with no previous pattern to repeat (an empty one, #1173),
/// or a `gn`/`gN` with none to match (#1184).
fn no_previous_pattern(cmd: &Command) -> bool {
    matches!(
        cmd,
        Command::Search { pattern, .. } | Command::SearchObject { pattern, .. } if pattern.is_empty()
    )
}

/// Whether `cmd` is a search whose pattern matches nowhere in the focused buffer; if so the status gets
/// Vim's `E486` (`:help E486`). The buffer-wide test is the `:g` marking rule, so it agrees with `:g` on
/// what matches; a pattern that can span a line break (`\n`, `\_x`) is left to the search itself.
fn search_misses(cmd: &Command, ws: &mut Workspace, status: &mut Status) -> bool {
    let pattern = match cmd {
        Command::Search { pattern, .. }
        | Command::SearchNext(pattern)
        | Command::SearchPrev(pattern) => pattern,
        _ => return false,
    };
    if pattern.is_empty() || pattern.contains("\\n") || pattern.contains("\\_") {
        return false;
    }
    match ws.global_marks(ruse_core::SubRange::WholeFile, pattern, false) {
        Ok(lines) if lines.is_empty() => {
            status.error(format!("E486: Pattern not found: {pattern}"));
            true
        }
        // A malformed pattern (`/\(`, `/a\{`) fails with the pattern reader's error, and nothing runs (#1087).
        Err(e @ ruse_core::RegexError::Syntax(_)) => {
            status.error(regex_error_msg(&e));
            true
        }
        _ => false,
    }
}

/// Resolve a Normal-mode INFO command (`ga`/`:ascii` → [`Command::AsciiInfo`], `CTRL-G` →
/// [`Command::FileInfo`], `g CTRL-G` → [`Command::CursorInfo`]) into its status-line message against the
/// focused buffer, or `None` for any other command. These print only — the buffer is never mutated — so the
/// run loop just sets `status` and skips dispatch. Splitting this out keeps the whole
/// keystroke→Command→message path unit-testable without a session.
/// `ga` is not kept in `:messages` and `CTRL-G` is (nvim v0.12.6); `g8` and `g CTRL-G` follow them.
fn info_status(cmd: &Command, ws: &Workspace) -> Option<Msg> {
    match cmd {
        Command::AsciiInfo => {
            let pane = ws.focused();
            Some(Msg::echo(ruse_core::ascii_info(
                pane.doc.bytes(),
                pane.view.cursor(),
            )))
        }
        Command::Utf8Info => {
            let pane = ws.focused();
            Some(Msg::echo(ruse_core::utf8_info(
                pane.doc.bytes(),
                pane.view.cursor(),
            )))
        }
        Command::FileInfo => {
            let id = ws.focused_buffer();
            let name = ws.buffer_name(id);
            let pane = ws.focused();
            Some(Msg::info(ruse_core::file_info(
                name,
                pane.doc.is_modified(),
                pane.doc.bytes(),
                pane.view.cursor(),
            )))
        }
        Command::CursorInfo => {
            let pane = ws.focused();
            // The `Col` vcol uses the View's `'tabstop'` (ruse's default is 4, not nvim's 8) — the same width
            // the renderer paints tabs at, so it reflects the on-screen layout the user actually sees.
            Some(Msg::info(ruse_core::cursor_pos_info(
                pane.doc.bytes(),
                pane.view.cursor(),
                pane.view.tabs(),
            )))
        }
        _ => None,
    }
}

/// Sync the four read-only special registers `"/ ": ". "%` of the focused view from the current frontend
/// state (`:help quote_/`), so a `"/p` / `C-r :` / `".p` / `"%p` dispatched next reads the live values:
/// `"/` from the search ring, `":` from the last executed Ex line, `".` from the last insert session, and
/// `"%` from the focused buffer's on-disk path (empty for an unnamed buffer). Called before dispatch.
/// What `:` completion reads from the session (#613): buffer names, the current and alternate file,
/// and the values of the options ruse keeps. Files come from the real file system (the default).
struct ExCompletion<'a> {
    ws: &'a Workspace,
    files: &'a Files,
    alt_file: &'a Option<crate::app::ex_more::AltFile>,
    fold_method: Option<crate::input::FoldMethod>,
    foldlevel: Option<usize>,
}

impl ExCompletion<'_> {
    fn file_name(&self, id: Option<DocumentId>) -> Option<String> {
        id.and_then(|id| self.files.get(&id))
            .map(|bf| bf.path.to_string_lossy().into_owned())
    }
}

impl crate::input::complete::CompletionContext for ExCompletion<'_> {
    fn buffer_names(&self) -> Vec<String> {
        self.ws.buffers().into_iter().map(|b| b.name).collect()
    }
    fn current_file(&self) -> Option<String> {
        self.file_name(Some(self.ws.focused_buffer()))
    }
    fn alternate_file(&self) -> Option<String> {
        crate::app::ex_more::alternate_name(self.alt_file, self.ws, self.files)
    }
    fn buffer_files(&self) -> Vec<(u64, String)> {
        self.files
            .iter()
            .map(|(id, bf)| (id.0, bf.path.to_string_lossy().into_owned()))
            .collect()
    }
    fn option_value(&self, long: &str) -> Option<String> {
        match long {
            "tabstop" => Some(self.ws.focused().view.tab_width().to_string()),
            "foldlevel" => Some(self.foldlevel.unwrap_or(0).to_string()),
            "foldmethod" => Some(
                match self.fold_method.unwrap_or(crate::input::FoldMethod::Manual) {
                    crate::input::FoldMethod::Indent => "indent",
                    crate::input::FoldMethod::Manual => "manual",
                }
                .into(),
            ),
            _ => None,
        }
    }
}

/// Sync the read-only registers `"/ ": ". "% "#` from frontend state. Each one whose text changed is
/// written through the trace (RFC-0020 R6), since no recorded command produced it; an unchanged one is
/// not recorded again.
fn sync_special_registers(
    ws: &mut Workspace,
    recorded: &mut Vec<Command>,
    engine: &InputEngine,
    last_ex_line: &Option<String>,
    files: &Files,
    alt_file: &Option<crate::app::ex_more::AltFile>,
) {
    // `"%` shows the file name as it was opened (a relative path, matching Vim's default `%`); an unnamed
    // buffer has no `Files` entry, so the slot stays empty.
    let file = files
        .get(&ws.focused_buffer())
        .map(|bf| bf.path.display().to_string());
    // `"#` is the alternate file name (#1022): the one a `:w {file}`, `:sav` or `:file` left (#610), or the
    // alternate buffer's file, shown the same way.
    let alternate = crate::app::ex_more::alternate_name(alt_file, ws, files);
    for (name, value) in [
        ('/', engine.last_search().map(str::to_string)),
        (':', last_ex_line.clone()),
        ('.', engine.last_inserted_text()),
        ('%', file),
        ('#', alternate),
    ] {
        // A pattern spells a byte that is not UTF-8 as the regex engine's escape char (`*` on such a
        // word, #1243); `"/` holds the byte itself, as nvim's does.
        let bytes = match (name, value) {
            ('/', Some(pat)) => ruse_core::pattern::unspell(&pat).into_owned(),
            (_, value) => value.unwrap_or_default().into_bytes(),
        };
        // Compared by text: an empty slot never written reads as an empty one (an empty special register
        // is "not set", E29/E30/E32/E35, whichever way it is empty).
        if ws.registers().get(Some(name)).text() != bytes.as_slice() {
            write_register(
                ws,
                recorded,
                Some(name),
                ruse_core::Register::charwise(bytes),
            );
        }
    }
}

/// Dispatch the key after a `C-w` prefix (F-007 MVP window commands): `w`/`C-w` focus next, `s` split
/// horizontally, `v` split vertically, `c` close focused, `q` = `:quit`. An unrecognised key is ignored
/// (Vim beeps). The full `C-w` family is post-MVP.
fn dispatch_window(
    key: crossterm::event::KeyEvent,
    count: u32,
    ws: &mut Workspace,
    quit: &mut bool,
    status: &mut Status,
) {
    // `{count}C-w w` / `{count}C-w W` go to window `count`, the last one when there are fewer (nvim 0.12.6:
    // `9<C-w>w` with three windows lands in window 3); without a count they step to the next / previous.
    let nth = |ws: &mut Workspace| {
        let n = (count as usize).min(ws.window_count());
        ws.focus_window(n - 1);
    };
    match key.code {
        KeyCode::Char('w' | 'W') if count > 0 => nth(ws),
        _ if is_ctrl(key, 'w') && count > 0 => nth(ws),
        _ if is_ctrl(key, 'w') => ws.focus_next(),
        KeyCode::Char('w') if key.modifiers.is_empty() => ws.focus_next(),
        KeyCode::Char('W') => ws.focus_prev(),
        KeyCode::Char('s') => {
            ws.split(SplitDir::Horizontal);
        }
        KeyCode::Char('v') => {
            ws.split(SplitDir::Vertical);
        }
        KeyCode::Char('c') => {
            if !ws.close_focused() {
                status.error("E444: Cannot close last window");
            }
        }
        // `C-w q` is `:quit`: close the focused window (its buffer stays loaded, hidden, as with nvim's
        // default 'hidden'), or on the last window quit through the same unsaved-changes check (E37/E162).
        KeyCode::Char('q') => crate::app::dispatch::quit_window(ws, status, quit, false),
        _ => {}
    }
}

#[cfg(test)]
mod journal_outcome_tests {
    use super::*;

    /// RFC-0023 §8 (#938): the first failure of a failing run names the error and the journal; the
    /// next failures are quiet; a success ends the run.
    #[test]
    fn journal_outcome_names_the_journal() {
        let id = Workspace::new(&b""[..]).focused_buffer();
        let doc = std::path::Path::new("f.txt");
        let journal = std::path::Path::new("/s/ruse/journal/f.txt.01.rj");
        let mut failed = HashSet::new();
        let mut status = crate::app::messages::Status::default();
        let fail = || Err(io::Error::from(io::ErrorKind::PermissionDenied));
        journal_outcome(fail(), id, doc, journal, &mut failed, &mut status);
        assert!(failed.contains(&id));
        assert!(
            status.starts_with("E303: unable to write the recovery journal for \"f.txt\": ")
                && status.ends_with(" (/s/ruse/journal/f.txt.01.rj)"),
            "{status}"
        );
        status.clear();
        journal_outcome(fail(), id, doc, journal, &mut failed, &mut status);
        assert_eq!(status, "", "once per failing run");
        journal_outcome(Ok(()), id, doc, journal, &mut failed, &mut status);
        assert!(!failed.contains(&id));
    }
}

#[cfg(test)]
mod info_tests {
    use super::info_status;
    use ruse_core::{Command, Workspace};

    // End-to-end for the frontend-resolve seam: a Workspace + cursor -> the exact status message. The
    // byte-precise format assertions live in `ruse_core::info`; here we prove the wiring reads the focused
    // buffer's bytes, cursor, name, and modified flag and routes each Command to the right helper.
    #[test]
    fn ascii_info_reads_char_under_cursor() {
        let mut ws = Workspace::new(&b"Hi"[..]);
        ws.place_focused_cursor(ruse_core::BytePos(1)); // on 'i'
        assert_eq!(
            info_status(&Command::AsciiInfo, &ws)
                .map(|m| m.text)
                .as_deref(),
            Some("<i>  105,  Hex 69,  Octal 151")
        );
    }

    #[test]
    fn file_info_uses_name_and_line_count() {
        let mut ws = Workspace::new(&b"one\ntwo\nthree\n"[..]);
        ws.set_focused_buffer_name("f.txt");
        ws.place_focused_cursor(ruse_core::BytePos(0)); // line 1 of 3 -> 33%
        assert_eq!(
            info_status(&Command::FileInfo, &ws)
                .map(|m| m.text)
                .as_deref(),
            Some("\"f.txt\" 3 lines --33%--")
        );
    }

    #[test]
    fn file_info_unnamed_buffer() {
        let mut ws = Workspace::new(&b"x\n"[..]);
        ws.place_focused_cursor(ruse_core::BytePos(0));
        assert_eq!(
            info_status(&Command::FileInfo, &ws)
                .map(|m| m.text)
                .as_deref(),
            Some("\"[No Name]\" 1 line --100%--")
        );
    }

    #[test]
    fn cursor_info_counts_over_the_buffer() {
        let mut ws = Workspace::new(&b"foo bar baz\n"[..]);
        ws.place_focused_cursor(ruse_core::BytePos(4)); // on 'b' of "bar" (byte 4, word 2)
        assert_eq!(
            info_status(&Command::CursorInfo, &ws)
                .map(|m| m.text)
                .as_deref(),
            Some("Col 5 of 11; Line 1 of 1; Word 2 of 3; Byte 5 of 12")
        );
    }

    #[test]
    fn non_info_command_is_none() {
        let ws = Workspace::new(&b"x\n"[..]);
        assert!(info_status(&Command::Undo, &ws).is_none());
    }
}

#[cfg(test)]
mod journal_tests {
    use super::*;

    /// A session on `f.txt` in a scratch directory whose buffer is journaled into the scratch state home,
    /// as the binary journals a file buffer (the test does not touch the process-global rescue store).
    fn journaled(
        tag: &str,
        text: &[u8],
    ) -> (Session, &'static persist::journal::Journals, PathBuf) {
        let dir = std::env::temp_dir().join(format!("ruse-session-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("f.txt");
        std::fs::write(&file, text).unwrap();
        let mut s = Session::new(SessionInit {
            path: Some(file.clone()),
            fmt: persist::encoding::FileFormat::default(),
            initial: text.to_vec(),
            status: String::new(),
            profile: "vim",
            term: TermInfo::default(),
            cwd: dir.clone(),
            trusted: false,
            rescue: false,
        });
        let j: &'static persist::journal::Journals = Box::leak(Box::new(
            persist::journal::Journals::in_state_home(&dir.join("state")),
        ));
        s.journals = Some(j);
        let id = s.ws.focused_buffer();
        let fmt = persist::encoding::FileFormat::default();
        let mut w = persist::journal::Writer::new(j, &file, fmt);
        w.absorb(
            s.ws.document_mut(id).unwrap(),
            fmt,
            std::time::Instant::now(),
        );
        s.journal_writers.insert(id, w);
        (s, j, file)
    }

    /// #1202: an LSP edit batch is a risky operation (RFC-0023 §4). The typing before it reaches the journal
    /// before the batch applies, with no idle wait.
    #[test]
    fn an_lsp_edit_batch_syncs_the_journal_first() {
        let (mut s, j, file) = journaled("lsp-batch", b"fn main() {}\n");
        let typed = vec![(BytePos(0), BytePos(0), "pub ".to_string())];
        assert!(s.ws.apply_edits(&typed, TransactionOrigin::UserInput));
        assert!(!j.path_for(&file).exists(), "nothing is due yet");
        s.lsp
            .queue_format_edits(vec![(BytePos(0), BytePos(0), "// f\n".to_string())]);
        let frame = s.pre_frame(80, 24);
        s.post_render(&frame);
        assert_eq!(
            j.replay(Some(&file)).as_deref(),
            Some(&b"pub fn main() {}\n"[..]),
            "the typing, synced before the batch"
        );
        assert_eq!(s.ws.focused().doc.bytes(), b"// f\npub fn main() {}\n");
        let _ = std::fs::remove_dir_all(file.parent().unwrap());
    }
}
