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
    /// `auto-save-visited-mode` saved since the last key (#619): once per idle period.
    auto_saved_since_input: bool,
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
    /// An Emacs question (save-some-buffers, yes-or-no-p, a file name), waiting for its key (#619).
    emacs_prompt: Option<emacs_files::EmacsPrompt>,
    /// The buffers in Emacs View mode, `true` when `read-only-mode` (with `view-read-only`) entered it.
    view_mode: HashMap<DocumentId, bool>,
    /// A `'confirm'` / `:confirm` dialog over unsaved changes, waiting for its key (#619).
    changed_prompt: Option<crate::app::abandon::ChangedPrompt>,
    /// The Ex command a window key stands for (`C-w q` is `:quit`, `C-w c` `:close`), run after routing.
    window_ex: Option<&'static str>,
    /// `:confirm` on the Ex command running, and that command's line as typed (modifiers included).
    ex_confirm: bool,
    ex_line: String,
    /// The key that answered [`Session::swap`], handed from the key routing to [`Session::answer_swap`].
    swap_answer: Option<KeyEvent>,
    /// The hlsearch pattern (last `/`-search), until `:noh`.
    pub(crate) search_hl: Option<String>,
    /// hlsearch/incsearch render preferences (`:set (no)hlsearch/incsearch`); ON by default (better-than-Vim,
    /// which defaults hlsearch off — a slow-terminal-era default; matches are viewport-cached here).
    pub(crate) hlsearch_on: bool,
    pub(crate) incsearch_on: bool,
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
        let mut files = Files::new();
        // Each profile writes by its own editor's rules (#619): nvim's backups, or Emacs's.
        files.write = crate::app::write_opts::WriteOptions::new(profile == "emacs");
        let mut highlighters: Highlighters = HashMap::new();
        if let Some(p) = path.as_ref() {
            ws.set_focused_buffer_name(p.display().to_string());
            attach_file(&mut files, &mut highlighters, file_buf, p.clone(), fmt);
            let len = ws.focused().doc.len();
            crate::app::dispatch::mark_large(&mut files, &mut highlighters, file_buf, len);
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
            auto_saved_since_input: false,
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
            changed_prompt: None,
            emacs_prompt: None,
            view_mode: HashMap::new(),
            window_ex: None,
            ex_confirm: false,
            ex_line: String::new(),
            swap_answer: None,
            search_hl: None,
            hlsearch_on: true,
            incsearch_on: true,
            scrolloff: SCROLLOFF,
            ui_opts: crate::app::set_exec::UiOptions {
                // Emacs re-reads a changed file only under auto-revert-mode, which is off (#619).
                buffers: crate::app::abandon::BufferOptions {
                    autoread: !emacs_profile,
                    ..Default::default()
                },
                ..crate::app::set_exec::UiOptions::default()
            },
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
    /// The per-frame work that precedes the render and the key read: snapshot the focused buffer, refresh
    /// the line index, reconcile folds, publish the crash-rescue state, follow a `:s///c` match, and scroll
    /// each pane to its cursor. `cols`×`term_rows` is the terminal size.
    pub(crate) fn pre_frame(&mut self, cols: u16, term_rows: u16) -> FrameCtx {
        // Snapshot the FOCUSED buffer once for the frame (line index, folds, highlight parse). `text_arc()`
        // is an Arc refcount bump (not an O(buffer) copy): a committed edit builds a fresh Arc, so this handle
        // keeps pointing at the bytes captured here even after the later `&mut ws` borrows edit the doc.
        let (revision, snapshot) = {
            let f = self.ws.focused();
            (f.doc.revision(), f.doc.text_arc())
        };
        // Refresh the line index (rebuilds only on a revision change) so the per-frame row/viewport
        // lookups below are O(log n), not an O(buffer) newline scan. MVP splits share this one buffer.
        self.line_idx
            .refresh_for(self.ws.focused_buffer(), revision, &snapshot);
        self.reconcile_folds(revision, &snapshot);
        self.publish_rescue();
        self.follow_confirm();
        let (text_rows, rects, wins) = self.scroll_windows(cols, term_rows);
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
        for ms in [
            self.journal_wake_ms(),
            self.disk_wake_ms(),
            self.auto_save_wake_ms(),
        ]
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
        if self.files.get(&id).is_some_and(|bf| bf.large) {
            flags.push_str("[large]");
        }
        if self.config.safe_mode.is_some() {
            flags.push_str("[Config: safe-mode]");
        }
        flags
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
        self.auto_saved_since_input = false;
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
            } else if let Some(prompt) = self.changed_prompt.take() {
                self.answer_changed(prompt, key, frame);
            } else if let Some(prompt) = self.emacs_prompt.take() {
                self.answer_emacs(prompt, key);
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
        self.auto_saved_since_input = false;
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
            Feed::Cmd(cmd) => self.run_cmd(cmd),
            Feed::Replay(cmds) => {
                for cmd in cmds {
                    self.run_cmd(cmd);
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
        self.confirm.is_some()
            || self.swap.is_some()
            || self.changed_prompt.is_some()
            || self.emacs_prompt.is_some()
            || self.overlay.is_some()
    }

    /// The focused window's `'scrolloff'` as the rows of context the viewport keeps.
    fn scrolloff_rows(&self) -> usize {
        scrolloff_for(self.ws.focused().view, self.scrolloff)
    }

    fn route(&mut self, raw_key: KeyEvent, from_replay: bool, frame: &FrameCtx) {
        // Emacs View mode (`view-read-only`) takes its keys between commands (#619).
        if self.emacs_profile
            && !self.overlay_open()
            && self.engine.emacs_idle()
            && self.view_key(raw_key, frame)
        {
            return;
        }
        self.route_key(raw_key, from_replay, frame);
        // A read-only buffer refuses a change in Emacs's words (#619).
        if self.emacs_profile && self.status.is_error() && self.status.text() == ruse_core::E21 {
            let name = self
                .ws
                .buffer_name(self.ws.focused_buffer())
                .unwrap_or("*scratch*");
            let base = std::path::Path::new(name)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            self.status
                .error(format!("Buffer is read-only: #<buffer {base}>"));
        }
        // A window key that stands for an Ex command (`CTRL-W q` = `:quit`) runs it now, before the next
        // key, through the Ex dispatch and its unsaved-changes gate (#619).
        if let Some(line) = self.window_ex.take() {
            self.status.clear();
            self.run_ex_pieces(vec![line.to_string()], frame);
        }
    }

    /// Run `cmd` through [`run_cmd`] with the session's own workspace, files, recording, status line and
    /// quit flag.
    fn run_cmd(&mut self, cmd: Command) {
        run_cmd(
            cmd,
            &mut self.ws,
            &self.files,
            &mut self.recorded,
            &mut self.status,
            &mut self.quit,
        );
    }

    fn route_key(&mut self, raw_key: KeyEvent, from_replay: bool, frame: &FrameCtx) {
        if raw_key.kind == KeyEventKind::Release {
            return;
        }
        // The owners, in the order they claim a key; the first that takes it ends the routing.
        let Some(key) = self.macro_key(raw_key, from_replay) else {
            return;
        };
        // F-014: any key dismisses a shown hover panel (a fresh `K` re-populates it after its response).
        self.lsp.clear_hover();
        if self.popup_owns(key, frame) || self.prompt_owns(key) || self.picker_owns(key, frame) {
            return;
        }
        #[cfg(unix)]
        if self.terminal_owns(key) {
            return;
        }
        // The focused pane's visible height (for the scroll / recenter commands).
        let focused_h = self.focused_height(frame);
        if self.zf_owns(key)
            || self.z_prefix_owns(key, frame, focused_h)
            || self.window_prefix_owns(key)
        {
            return;
        }
        let at = self.key_ctx();
        if self.normal_chord_owns(key, at, frame, focused_h)
            || self.insert_chord_owns(key, frame, focused_h)
            || self.screen_line_owns(key, at, frame, focused_h)
        {
            return;
        }
        self.feed_engine(key, frame);
    }

    /// The focused pane's visible height in rows.
    fn focused_height(&self, frame: &FrameCtx) -> usize {
        frame
            .rects
            .get(self.ws.focus())
            .map_or(frame.text_rows as usize, |r| r.h as usize)
    }
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

const E35: &str = "E35: No previous regular expression";
/// `zf` or `:fold` while 'foldmethod' makes the folds (nvim's `e_cant_create_fold_with_current_foldmethod`).
const E350: &str = "E350: Cannot create fold with current 'foldmethod'";

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
    window_ex: &mut Option<&'static str>,
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
        // `C-w c` is `:close` and `C-w q` (`C-w C-q`) is `:quit` (`:help CTRL-W_c`): they run as those Ex
        // commands once routing is done, so 'hidden', 'autowrite' and 'confirm' apply as they do (#619).
        KeyCode::Char('c') if key.modifiers.is_empty() => *window_ex = Some("close"),
        KeyCode::Char('q') if key.modifiers.is_empty() => *window_ex = Some("quit"),
        _ if is_ctrl(key, 'q') => *window_ex = Some("quit"),
        _ => {}
    }
}

mod commands;
mod config_apply;
mod disk_watch;
mod emacs_files;
mod event_loop;
pub(crate) use event_loop::run;
mod ex_exec;
mod ex_line;
mod frame;
mod keys;
mod nested;
mod normal_keys;
use commands::*;
use disk_watch::*;

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
            info_status(&Command::AsciiInfo, &ws, true)
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
            info_status(&Command::FileInfo, &ws, true)
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
            info_status(&Command::FileInfo, &ws, true)
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
            info_status(&Command::CursorInfo, &ws, true)
                .map(|m| m.text)
                .as_deref(),
            Some("Col 5 of 11; Line 1 of 1; Word 2 of 3; Byte 5 of 12")
        );
    }

    #[test]
    fn non_info_command_is_none() {
        let ws = Workspace::new(&b"x\n"[..]);
        assert!(info_status(&Command::Undo, &ws, true).is_none());
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
