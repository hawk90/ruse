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
    record_register_writes, regex_error_msg, run_ex, run_repeat, sub_outcome, substitute_cmd,
    Files, Highlighters,
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

/// Rows of context kept above and below the cursor when scrolling (Vim's `scrolloff`): ruse's default
/// (`editor.scrolloff`), until `:set so` changes [`Session::scrolloff`].
const SCROLLOFF: i64 = 3;

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
    /// The windows' rows ([`crate::ui::layout::BottomLayout::text_rows`]).
    pub(crate) text_rows: u16,
    /// The screen's rows.
    pub(crate) rows: u16,
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

/// The editor state the event loop owns (#878): everything `run` used to keep in locals except the
/// terminal (guard, output, previous frame, graphics placements). `run` drives it with
/// [`Session::pre_frame`] → render → [`Session::post_render`] → read a key → [`Session::on_key`]; the
/// headless harness (`crate::headless`) drives the same methods without a terminal.
pub(crate) struct Session {
    /// What a semantic command runs against: the workspace, its files, the recording, the status line and
    /// the quit flag (#1509).
    pub(crate) ed: Editing,
    pub(crate) highlighters: HashMap<DocumentId, highlight::CachedHighlight>,
    /// Revision-cached line index: O(log n) row/line lookups instead of an O(buffer) newline scan per
    /// pane per frame (D-042 win D). Refreshed by `pre_frame`.
    pub(crate) line_idx: line_index::LineIndex,
    /// The line indexes of the other buffers the windows show (#1182), refreshed by `pre_frame`.
    pub(crate) pane_idx: line_index::PaneIndexes,
    pub(crate) initial: Vec<u8>,
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

/// The start workspace over `initial`, with its file registry and highlighters, and the start buffer's id
/// (the one that owns the session file).
fn start_workspace(
    initial: Vec<u8>,
    path: Option<&PathBuf>,
    fmt: persist::encoding::FileFormat,
    emacs_profile: bool,
) -> (Workspace, Files, Highlighters, DocumentId) {
    // F-007: the frontend now drives a Workspace (buffers + views + windows), not a single
    // EditorState. With one Window this is byte-identical to the pre-Workspace path; `:split`/
    // `:vsplit` open more Windows onto the same buffer with independent cursors and scroll.
    let mut ws = Workspace::new(initial);
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
    files.write = crate::app::write_opts::WriteOptions::new(emacs_profile);
    let mut highlighters: Highlighters = HashMap::new();
    if let Some(p) = path {
        ws.set_focused_buffer_name(p.display().to_string());
        attach_file(&mut files, &mut highlighters, file_buf, p.clone(), fmt);
        let len = ws.focused().doc.len();
        crate::app::dispatch::mark_large(&mut files, &mut highlighters, file_buf, len);
    }
    // Caret gravity follows the profile (D-050 / RFC-0015): the Emacs profile rests point BETWEEN chars
    // (line/buffer end = after the last char), so its edits are not Vim-clamped. Vim keeps `OnChar`, and
    // Native reuses the Vim text grammar (NAT-1) so it keeps `OnChar` too.
    if emacs_profile {
        ws.set_caret_gravity(CaretGravity::BetweenChar);
    }
    (ws, files, highlighters, file_buf)
}

/// Profile selection (F-012 / F-013): `emacs` = Emacs, `native` = Native, else Vim.
fn profile_engine(profile: &str) -> InputEngine {
    match profile {
        "emacs" => InputEngine::emacs(),
        "native" => InputEngine::native(),
        _ => InputEngine::new(),
    }
}

/// The UI options a session starts with.
fn start_ui_opts(emacs_profile: bool) -> crate::app::set_exec::UiOptions {
    crate::app::set_exec::UiOptions {
        // Emacs re-reads a changed file only under auto-revert-mode, which is off (#619).
        buffers: crate::app::abandon::BufferOptions {
            autoread: !emacs_profile,
            ..Default::default()
        },
        ..crate::app::set_exec::UiOptions::default()
    }
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
        let emacs_profile = profile == "emacs";
        let (ws, files, highlighters, file_buf) =
            start_workspace(initial.clone(), path.as_ref(), fmt, emacs_profile);
        let start_options = ws.focused().view.editing_options();
        let start_registers = ws.registers().filled();
        let trace_watch = crate::app::trace_check::TraceWatch::new(&ws, &files);
        Session {
            ed: Editing {
                ws,
                files,
                recorded: Vec::new(),
                // The startup line is shown, not kept (nvim keeps no banner).
                status: Status::showing(Msg::echo(status)),
                quit: false,
            },
            highlighters,
            line_idx: line_index::LineIndex::default(),
            pane_idx: line_index::PaneIndexes::default(),
            initial,
            start_options,
            start_registers,
            trace_watch,
            journals: rescue.then(persist::journal::current).flatten(),
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
            engine: profile_engine(profile),
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
            ui_opts: start_ui_opts(emacs_profile),
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
        crate::app::dispatch::apply_recovery(&mut self.ed.ws, rec, &mut self.ed.recorded);
        crate::app::dispatch::place_cursor(&mut self.ed.ws, &mut self.ed.recorded, 0);
    }
}

mod cmdline_ctx;
mod commands;
mod config_apply;
mod disk_watch;
mod editing;
mod emacs_files;
mod event_loop;
pub(crate) use event_loop::run;
#[cfg(test)]
mod config_defaults_tests;
mod ex_exec;
mod ex_helpers;
mod ex_line;
mod frame;
mod keys;
mod nested;
mod normal_keys;
mod replay;
mod route;
mod scroll;
use cmdline_ctx::{sync_special_registers, ExCompletion};
use commands::*;
use disk_watch::*;
pub(crate) use editing::Editing;
use ex_helpers::{
    give_report, goto_line, read_text, runs_external, to_i64, with_shell_failure, SwapPrompt, E35,
    E350,
};
pub(crate) use scroll::ScrollValid;
use scroll::{
    insert_page_key, page_key, resolve_screen_motion, run_scroll, scrolloff_for, viewport_line,
    ScrollKey,
};

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
    ) -> (
        Session,
        &'static persist::journal::Journals,
        PathBuf,
        crate::TestDir,
    ) {
        let dir = crate::TestDir::new(&format!("session-{tag}"));
        let file = dir.join("f.txt");
        std::fs::write(&file, text).unwrap();
        let mut s = Session::new(SessionInit {
            path: Some(file.clone()),
            fmt: persist::encoding::FileFormat::default(),
            initial: text.to_vec(),
            status: String::new(),
            profile: "vim",
            term: TermInfo::default(),
            cwd: dir.to_path_buf(),
            trusted: false,
            rescue: false,
        });
        let j: &'static persist::journal::Journals = Box::leak(Box::new(
            persist::journal::Journals::in_state_home(&dir.join("state")),
        ));
        s.journals = Some(j);
        let id = s.ed.ws.focused_buffer();
        let fmt = persist::encoding::FileFormat::default();
        let mut w = persist::journal::Writer::new(j, &file, fmt);
        w.absorb(
            s.ed.ws.document_mut(id).unwrap(),
            fmt,
            std::time::Instant::now(),
        );
        s.journal_writers.insert(id, w);
        (s, j, file, dir)
    }

    /// #1202: an LSP edit batch is a risky operation (RFC-0023 §4). The typing before it reaches the journal
    /// before the batch applies, with no idle wait.
    #[test]
    fn an_lsp_edit_batch_syncs_the_journal_first() {
        let (mut s, j, file, _dir) = journaled("lsp-batch", b"fn main() {}\n");
        let typed = vec![(BytePos(0), BytePos(0), "pub ".to_string())];
        assert!(s.ed.ws.apply_edits(&typed, TransactionOrigin::UserInput));
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
        assert_eq!(
            s.ed.ws.focused().doc.bytes(),
            b"// f
pub fn main() {}
"
        );
    }
}
