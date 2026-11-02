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

use ruse_core::{CaretGravity, Command, DocumentId, Mode, OpenKind, SplitDir, Workspace};

use crate::app::dispatch::{is_ctrl, open_file_into_buffer, run_cmd, run_ex, BufferFile, Files};
use crate::app::lsp_coordinator::LspCoordinator;
use crate::input::{parse_ex, Ex, Feed, InputEngine, Pending, ReadSource};
use crate::terminal::guard::TermGuard;
use crate::ui::palette::focused_context;
use crate::ui::picker::{PickOutcome, Picker};
use crate::ui::prompts::{confirm_key, confirm_prompt, prompt_recovery, Confirm, RecoveryChoice};
use crate::ui::render::{render, search_pattern};
use crate::ui::{
    action_picker, buffer_picker, diag_picker, file_picker,
    layout::{window_rects, Rect},
    line_picker, marks_picker, palette, pos_picker, ref_picker, register_picker,
};
use crate::{graphics, health, highlight, indent, line_index, persist, recover, screen, viewport};
#[cfg(unix)]
use crate::{pty, term_buffer};

/// Rows of context kept above and below the cursor when scrolling (Vim's `scrolloff`).
const SCROLLOFF: usize = 3;

/// Append a recovery-journal frame after every Nth change (revision bump) of a file buffer — a coarse
/// hard-kill safety net. Idle ticks never write (#858). A panic, fatal signal or I/O error journals every
/// modified buffer's exact latest state separately (`recover`). Full incremental journaling is post-MVP.
const JOURNAL_THROTTLE: u32 = 8;

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
    /// `C-w` (Normal mode): the next key is a window command (F-007).
    Window,
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
    let tree_of = |ws: &Workspace| {
        highlighters
            .get(&ws.focused_buffer())
            .and_then(highlight::CachedHighlight::tree)
    };
    // `=` — resolve the reindent range + levels from the tree, else fall through to the core `=`.
    let cmd = if let Command::Reindent { count, motion } = cmd {
        match (ws.reindent_range(motion, count), tree_of(ws)) {
            (Some((first_line, last_line)), Some(tree)) => Command::SetIndents {
                first_line,
                last_line,
                levels: indent::indent_levels(tree, snapshot, first_line, last_line),
            },
            _ => Command::Reindent { count, motion },
        }
    } else {
        cmd
    };
    // `o`/`O`/`<CR>` — seed the opened line with the tree-suggested indent. Dot-repeat replays the plain
    // open (column-0), so a stale recorded level is never re-applied at a new location.
    let cmd = if matches!(
        cmd,
        Command::OpenBelow | Command::OpenAbove | Command::InsertNewline
    ) {
        match tree_of(ws) {
            Some(tree) => {
                let cur = ws.focused().view.cursor();
                let row = line_idx.line_of(cur);
                let (kind, at, new_row) = match cmd {
                    Command::OpenBelow => {
                        let next = line_idx.nth_line_start(row + 1);
                        let le = if next > 0 && snapshot.get(next - 1) == Some(&b'\n') {
                            next - 1
                        } else {
                            snapshot.len()
                        };
                        (OpenKind::Below, le, row + 1)
                    }
                    Command::OpenAbove => (OpenKind::Above, line_idx.nth_line_start(row), row),
                    _ => (OpenKind::Split, cur, row + 1),
                };
                let level = indent::suggest_indent(tree, at, new_row);
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
        Command::InsertChar(c @ ('}' | ')' | ']')) if tree_of(ws).is_some() => {
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
}

/// The values one frame computes before its key is read and the key's handlers read back (#878): the
/// focused buffer's revision and byte snapshot (taken before any `&mut ws` edit of this key) and the pane
/// geometry. Built by [`Session::pre_frame`], so every key — a macro-replayed one too — sees a fresh frame.
pub(crate) struct FrameCtx {
    pub(crate) revision: ruse_core::Revision,
    pub(crate) snapshot: std::sync::Arc<Vec<u8>>,
    pub(crate) rects: Vec<Rect>,
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
    /// Publish every recoverable buffer to the crash-rescue store and throttle-journal file buffers each
    /// frame. The terminal session does; a headless test session does not touch that process-global state.
    pub(crate) rescue: bool,
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
    pub(crate) initial: Vec<u8>,
    pub(crate) recorded: Vec<Command>,
    /// Per-buffer journal throttles (#858): a frame is appended only after the buffer actually changed.
    journal_throttle: HashMap<DocumentId, persist::journal::Throttle>,
    /// Buffers whose last journal append failed, so the E303 warning shows once per failing run (#907).
    journal_failed: HashSet<DocumentId>,
    /// `ruse` with no file: the start buffer has no path, but its journal (`.ruse-journal`) is what the next
    /// pathless start replays, so a crash rescues it there (#856). Not throttle-journaled: it can never be
    /// saved, so a periodic journal would outlive every `:q!`.
    unnamed_start: Option<DocumentId>,
    rescue: bool,
    pub(crate) profile_name: &'static str,
    pub(crate) emacs_profile: bool,
    pub(crate) engine: InputEngine,
    pub(crate) quit: bool,
    term: TermInfo,
    /// The armed session key prefix, if any (`C-w`, `z`, `zf`, `[`/`]`, `CTRL-\`).
    prefix: Prefix,
    /// Folds, keyed by buffer (per-buffer, not per-window). Closed folds collapse in render; the cursor/scroll
    /// skip them. Under `foldmethod=manual` (default) edits shift/drop them; under `foldmethod=indent` (slice 3)
    /// they are RECOMPUTED from indentation on edit.
    pub(crate) folds: HashMap<DocumentId, Vec<crate::folds::Fold>>,
    /// Per-buffer line count from the previous frame, to detect edit-driven line shifts for manual fold ranges.
    fold_lines: HashMap<DocumentId, usize>,
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
    /// A `:s///c` interactive confirm loop, when active (F-009).
    pub(crate) confirm: Option<Confirm>,
    /// The hlsearch pattern (last `/`-search), until `:noh`.
    pub(crate) search_hl: Option<String>,
    /// hlsearch/incsearch render preferences (`:set (no)hlsearch/incsearch`); ON by default (better-than-Vim,
    /// which defaults hlsearch off — a slow-terminal-era default; matches are viewport-cached here).
    pub(crate) hlsearch_on: bool,
    pub(crate) incsearch_on: bool,
    /// `:set (no)fixeol` write preference; OFF by default — byte-preserve (no silent fixeol) is the honest
    /// default. When ON, `:w` ADDS a trailing `\n` if the buffer lacks one (Vim's fixendofline).
    fixeol_on: bool,
    /// The last `:s` (pattern, replacement, flags) — recorded on every substitute so `&` can repeat it (F-009).
    last_substitute: Option<(String, String, ruse_core::SubFlags)>,
    /// The last executed Ex command-line (WITHOUT the leading `:`) — Vim's `":` register (`:help quote_:`),
    /// read by `C-r :`. Recorded on every `Feed::ExecuteEx`.
    last_ex_line: Option<String>,
    /// The modal picker overlays (F-004 / F-013 NAT-3), all `Picker<T>` over different payloads: command
    /// palette (`C-p`), buffer-line jump (`C-l`), buffer switch (`C-b`), file finder (`C-f`). At most one is open.
    pub(crate) palette: Option<Picker<Command>>,
    pub(crate) line_picker: Option<Picker<usize>>,
    pub(crate) buffer_picker: Option<Picker<DocumentId>>,
    pub(crate) file_picker: Option<Picker<PathBuf>>,
    /// F-014: the LSP references picker — `(uri, line, character)` locations; Enter jumps (same/cross file).
    pub(crate) ref_picker: Option<Picker<(String, u32, u32)>>,
    /// F-014: the LSP code-action picker — Enter applies the selected action's WorkspaceEdit.
    pub(crate) action_picker: Option<Picker<crate::lsp::protocol::CodeAction>>,
    /// F-014: the diagnostics list picker — payload is the diagnostic's start byte; Enter jumps there.
    pub(crate) diag_picker: Option<Picker<usize>>,
    /// F-029: the `:registers` viewer — payload is the register name; view-only (Enter just closes).
    pub(crate) reg_picker: Option<Picker<char>>,
    /// `:digraphs` listing — payload is the glyph; view-only (Enter just closes), like `reg_picker`.
    pub(crate) digraph_picker: Option<Picker<char>>,
    /// `[I`/`]I` keyword-line listing — payload is the match's 1-based line number; view-only (Enter closes).
    pub(crate) keyword_lines_picker: Option<Picker<usize>>,
    /// F-003: the `:marks` viewer — payload is the mark's byte offset; Enter jumps the cursor there.
    pub(crate) marks_picker: Option<Picker<usize>>,
    /// F-003: the shared `:jumps` / `:changes` position viewer — payload is a byte offset; Enter jumps.
    pub(crate) pos_picker: Option<Picker<usize>>,
    /// F-011: live terminal buffers keyed by their placeholder DocumentId (unix-only). `Prefix::TermEscape`
    /// tracks a `CTRL-\` awaiting `CTRL-N` (the Terminal → Terminal-Normal escape).
    #[cfg(unix)]
    pub(crate) terminals: HashMap<DocumentId, term_buffer::Terminal>,
    /// F-014: all app-side LSP orchestration (clients, diagnostics, hover/completion, request dispatch,
    /// deferred edit-applies) lives behind the coordinator; the loop just drives it (CAP-LSP-COORD).
    pub(crate) lsp: LspCoordinator,
    pub(crate) status: String,
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
        let mut highlighters: HashMap<DocumentId, highlight::CachedHighlight> = HashMap::new();
        if let Some(p) = path.as_ref() {
            ws.set_focused_buffer_name(p.display().to_string());
            files.insert(
                file_buf,
                BufferFile {
                    path: p.clone(),
                    fmt,
                },
            );
            if let Some(h) = p
                .extension()
                .and_then(|e| e.to_str())
                .and_then(highlight::CachedHighlight::for_ext)
            {
                highlighters.insert(file_buf, h);
            }
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
        Session {
            ws,
            files,
            highlighters,
            line_idx: line_index::LineIndex::default(),
            initial,
            recorded: Vec::new(),
            journal_throttle: HashMap::new(),
            journal_failed: HashSet::new(),
            unnamed_start: path.is_none().then_some(file_buf),
            rescue,
            profile_name: profile,
            emacs_profile,
            engine,
            quit: false,
            term,
            prefix: Prefix::None,
            folds: HashMap::new(),
            fold_lines: HashMap::new(),
            fold_method: HashMap::new(),
            foldlevel: HashMap::new(),
            foldenable: HashMap::new(),
            fold_rev: HashMap::new(),
            meta_tail: None,
            macros: crate::keys::MacroState::new(),
            confirm: None,
            search_hl: None,
            hlsearch_on: true,
            incsearch_on: true,
            fixeol_on: false,
            last_substitute: None,
            last_ex_line: None,
            palette: None,
            line_picker: None,
            buffer_picker: None,
            file_picker: None,
            ref_picker: None,
            action_picker: None,
            diag_picker: None,
            reg_picker: None,
            digraph_picker: None,
            keyword_lines_picker: None,
            marks_picker: None,
            pos_picker: None,
            #[cfg(unix)]
            terminals: HashMap::new(),
            lsp: LspCoordinator::new(cwd, trusted),
            status,
        }
    }

    /// The per-frame work that precedes the render and the key read: snapshot the focused buffer, refresh
    /// the line index, reconcile folds, publish the crash-rescue state, follow a `:s///c` match, and scroll
    /// each pane to its cursor. `cols`×`term_rows` is the terminal size.
    pub(crate) fn pre_frame(&mut self, cols: u16, term_rows: u16) -> FrameCtx {
        let Session {
            ws,
            files,
            line_idx,
            journal_throttle,
            journal_failed,
            unnamed_start,
            rescue,
            folds,
            fold_lines,
            fold_method,
            foldlevel,
            foldenable,
            fold_rev,
            confirm,
            status,
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
        line_idx.refresh(revision, &snapshot);
        // Fold reconcile (per frame): (a) shift/drop folds when the buffer's line count changed since
        // last frame — approximating the edit point as the cursor line; (b) snap the cursor OUT of any closed
        // fold onto its summary (start) line, so it never rests on a hidden row.
        {
            let buf = ws.focused_buffer();
            let nlines = line_idx.line_of(snapshot.len());
            let indent = fold_method.get(&buf).copied() == Some(crate::input::FoldMethod::Indent);
            if indent {
                // Indent folds are DERIVED: recompute from indentation whenever the buffer content changed
                // (revision bump), then apply the foldlevel/foldenable rule. This resets any per-fold
                // za/zo/zc toggle on edit — an honest first slice; foldlevel drives closed state between
                // edits (zr/zm/zR/zM and :set foldlevel re-apply immediately, below).
                if fold_rev.get(&buf) != Some(&revision) {
                    let tw = ws.focused().view.tab_width();
                    let mut computed = crate::folds::compute_indent_folds(&snapshot, tw);
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
            } else if let Some(fv) = folds.get_mut(&buf) {
                // Manual folds are NOT recomputed: shift/drop them when the line count changed since last
                // frame (approximating the edit point as the cursor line).
                if !fv.is_empty() {
                    let prev = *fold_lines.get(&buf).unwrap_or(&nlines);
                    if nlines != prev {
                        let delta = nlines as isize - prev as isize;
                        let at = line_idx.line_of(ws.focused().view.cursor());
                        crate::folds::shift(fv, at, delta);
                    }
                }
            }
            // Snap the cursor OUT of any closed fold onto its summary (start) row (both methods).
            if let Some(fv) = folds.get(&buf) {
                let cline = line_idx.line_of(ws.focused().view.cursor());
                let snapped = crate::folds::snap_out(fv, cline);
                if snapped != cline {
                    ws.place_focused_cursor(line_idx.nth_line_start(snapped));
                }
            }
            fold_lines.insert(buf, nlines);
        }
        // Crash-rescue store + recovery journal cover EVERY recoverable buffer, not just the focused one
        // (#856): each file-backed buffer (keyed by its own path, F-007; incl. `:e`-opened ones) and the
        // pathless start buffer. A scratch buffer (`:enew`) is skipped so its bytes never land in some
        // file's recovery. `save()` clears a buffer's journal by path, so this stays consistent.
        if *rescue {
            let mut entries = Vec::with_capacity(files.len() + 1);
            for (id, bf) in files.iter() {
                let Some(doc) = ws.document(*id) else {
                    continue;
                };
                let (rev, dirty) = (doc.revision().0, doc.is_modified());
                if journal_throttle
                    .entry(*id)
                    .or_default()
                    .tick(rev, dirty, JOURNAL_THROTTLE)
                {
                    // A failed append (read-only or vanished directory, full disk) is shown once per
                    // failing run, as Neovim's E303 is, rather than silently leaving the work unprotected.
                    match persist::journal::append(Some(bf.path.as_path()), doc.bytes()) {
                        Ok(()) => {
                            journal_failed.remove(id);
                        }
                        Err(e) => {
                            if journal_failed.insert(*id) {
                                tracing::warn!(event = "journal.failed", path = %bf.path.display(), error = %e);
                                *status = format!(
                                    "E303: unable to write the recovery journal for \"{}\": {e}",
                                    bf.path.display()
                                );
                            }
                        }
                    }
                }
                entries.push(recover::Entry {
                    path: Some(bf.path.clone()),
                    bytes: doc.text_arc(),
                    modified: dirty,
                });
            }
            if let Some(doc) = unnamed_start.and_then(|id| ws.document(id)) {
                entries.push(recover::Entry {
                    path: None,
                    bytes: doc.text_arc(),
                    modified: doc.is_modified(),
                });
            }
            journal_throttle.retain(|id, _| files.contains_key(id) && ws.doc_is_live(*id));
            recover::sync(entries);
        }
        // During a `:s///c` confirm, follow the current match so the viewport scrolls it into view.
        if let Some(c) = &confirm {
            if let Some(s) = c.subs.get(c.idx) {
                ws.place_focused_cursor(s.start);
                *status = confirm_prompt(c);
            }
        }
        // Per-window viewport pass: scroll each pane so ITS cursor stays visible in ITS rectangle
        // (F-007 acceptance #1 — independent scroll). Geometry is shared with render below.
        let text_rows = term_rows.saturating_sub(1);
        let rects = window_rects(cols, text_rows, ws.window_count(), ws.split_dir());
        for (i, rect) in rects.iter().enumerate() {
            let (cursor_row, cur_top) = {
                let p = ws.pane(i);
                (line_idx.line_of(p.view.cursor()), p.view.top())
            };
            let new_top = viewport::scroll_top(cursor_row, rect.h as usize, SCROLLOFF, cur_top);
            ws.set_top(i, new_top);
        }
        FrameCtx {
            revision,
            snapshot,
            rects,
            cols,
            text_rows,
        }
    }

    /// The per-frame work after the render: apply the LSP coordinator's deferred results (opening a buffer
    /// mutates `highlighters`, which the frame's highlight spans borrowed until render ended), then open the
    /// references / code-action pickers it collected.
    pub(crate) fn post_render(&mut self, frame: &FrameCtx) {
        self.lsp.apply_pending(
            &mut self.ws,
            &mut self.files,
            &mut self.highlighters,
            &frame.snapshot,
            &mut self.status,
        );
        if let Some(locs) = self.lsp.take_refs() {
            self.ref_picker = Some(ref_picker::open(locs, self.lsp.cwd()));
        }
        if let Some(actions) = self.lsp.take_actions() {
            self.action_picker = Some(action_picker::open(actions));
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
        poll_ms
    }

    /// The next key the session queued for itself, if any, with its `from_replay` flag: the key half of a
    /// split Alt chord (typed, #896), else the next key of a macro replay (`@{reg}`, `@:`). The caller runs
    /// a full frame before each one and feeds it to [`Session::on_key`] before reading the terminal.
    pub(crate) fn next_queued(&mut self) -> Option<(KeyEvent, bool)> {
        match self.meta_tail.take() {
            Some(k) => Some((k, false)),
            None => self.macros.next_replay().map(|k| (k, true)),
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
        self.route(raw_key, from_replay, frame);
        if self.quit {
            Flow::Quit
        } else {
            Flow::Continue
        }
    }

    /// Whether a modal overlay owns the keystream: a `:s///c` confirm loop or an open picker. The macro
    /// layer must not take its `q`/`@` (#851).
    fn overlay_open(&self) -> bool {
        self.confirm.is_some()
            || self.palette.is_some()
            || self.line_picker.is_some()
            || self.buffer_picker.is_some()
            || self.file_picker.is_some()
            || self.ref_picker.is_some()
            || self.action_picker.is_some()
            || self.diag_picker.is_some()
            || self.reg_picker.is_some()
            || self.digraph_picker.is_some()
            || self.keyword_lines_picker.is_some()
            || self.marks_picker.is_some()
            || self.pos_picker.is_some()
    }

    fn route(&mut self, raw_key: KeyEvent, from_replay: bool, frame: &FrameCtx) {
        let overlay = self.overlay_open();
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
            search_hl,
            last_substitute,
            last_ex_line,
            palette,
            line_picker,
            buffer_picker,
            file_picker,
            ref_picker,
            action_picker,
            diag_picker,
            reg_picker,
            digraph_picker,
            keyword_lines_picker,
            marks_picker,
            pos_picker,
            #[cfg(unix)]
            terminals,
            lsp,
            status,
            profile_name,
            meta_tail,
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
        // that key. `q` also records and stops from Visual; `@`/`Q` are Normal-only.
        let boundary =
            !overlay && matches!(prefix, Prefix::None) && engine.pending() == Pending::Base;
        let mode = ws.focused().view.mode();
        let macro_normal = boundary && matches!(mode, Mode::Normal);
        let macro_q = boundary && matches!(mode, Mode::Normal | Mode::Visual { .. });
        let step = macros.step_gated(raw_key, from_replay, macro_normal, macro_q);
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
            // `q:` / `q/` / `q?` — open the command-line window (`:help cmdwin`) over that history ring.
            crate::keys::Step::OpenCmdWin(kind) => {
                engine.open_cmdwin(kind);
                return;
            }
            crate::keys::Step::Store(reg, bytes) => {
                let n = bytes.len();
                ws.set_register_raw(Some(reg), bytes);
                *status = format!("recorded @{reg} ({n} keys)");
                return;
            }
            crate::keys::Step::Replay(reg) => {
                // `{count}@{reg}` repeats the macro `count` times; the count was accumulated by the engine
                // from the digits typed before `@`, so consume it here. The recursion budget is shared
                // across all copies, so `999@a` of a big macro still terminates.
                let n = engine.take_count().max(1);
                let bytes = ws.register_bytes(Some(reg));
                for _ in 0..n {
                    if !macros.replay(&bytes) {
                        *status = "macro replay aborted (key limit)".to_string();
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
                                *status = "macro replay aborted (key limit)".to_string();
                                break;
                            }
                        }
                    }
                    None => *status = "E30: No previous command line".to_string(),
                }
                return;
            }
        };
        // F-014: any key dismisses a shown hover panel (a fresh `K` re-populates it after its response).
        lsp.clear_hover();
        // F-014 #5: the completion pum owns the keystream while open (nav / accept / dismiss); `<C-x><C-o>`
        // in Vim/Native Insert opens it. Each returns whether it consumed the key (so the loop `continue`s).
        if lsp.on_completion_key(key, ws, status) {
            return;
        }
        if lsp.on_omni_key(key, ws, files, snapshot, revision, *emacs_profile, status) {
            return;
        }
        // A `:s///c` confirm loop owns the keystream while active: y/n/a/l/q per match (F-009 #2).
        if confirm.is_some() {
            confirm_key(confirm, key, ws, status);
            return;
        }
        // A picker owns the keystream while open (F-004 / F-013 NAT-3): its transient keymap filters/moves,
        // Enter accepts (the caller runs the payload-specific action), Esc closes. At most one is open.
        // The line picker jumps the cursor to the selected line's byte offset.
        if let Some(outcome) = line_picker.as_mut().map(|p| p.on_key(key)) {
            if let PickOutcome::Accept = outcome {
                let offset = line_picker.as_ref().and_then(|p| p.selected().copied());
                if let Some(offset) = offset {
                    ws.place_focused_cursor(offset);
                }
            }
            if !matches!(outcome, PickOutcome::Continue) {
                *line_picker = None;
            }
            return;
        }
        // The buffer picker switches the focused window to the selected buffer.
        if let Some(outcome) = buffer_picker.as_mut().map(|p| p.on_key(key)) {
            if let PickOutcome::Accept = outcome {
                if let Some(id) = buffer_picker.as_ref().and_then(|p| p.selected().copied()) {
                    ws.focus_buffer(id);
                }
            }
            if !matches!(outcome, PickOutcome::Continue) {
                *buffer_picker = None;
            }
            return;
        }
        // The file picker opens the selected path into a new buffer (the interactive form of `:e`).
        if let Some(outcome) = file_picker.as_mut().map(|p| p.on_key(key)) {
            if let PickOutcome::Accept = outcome {
                if let Some(p) = file_picker.as_ref().and_then(|p| p.selected().cloned()) {
                    *status =
                        open_file_into_buffer(&p.display().to_string(), ws, files, highlighters)
                            .status;
                }
            }
            if !matches!(outcome, PickOutcome::Continue) {
                *file_picker = None;
            }
            return;
        }
        // F-014: the references picker jumps to the selected location. The `spans` borrow is released by
        // now (render is done), so opening a cross-file buffer here is free — no defer needed (unlike the
        // response-loop goto). Same file → move; other file → open then move (the goto path, inline).
        if let Some(outcome) = ref_picker.as_mut().map(|p| p.on_key(key)) {
            if let PickOutcome::Accept = outcome {
                if let Some((uri, l, c)) = ref_picker.as_ref().and_then(|p| p.selected().cloned()) {
                    // The open helper focuses the buffer already holding this file (#848), so the
                    // current file and another open file both move without a second Document.
                    let path = crate::lsp::uri_to_path(&uri);
                    let target = std::fs::canonicalize(&path).unwrap_or(path);
                    let opened = open_file_into_buffer(
                        &target.display().to_string(),
                        ws,
                        files,
                        highlighters,
                    );
                    if opened.id.is_some() {
                        let bytes = ws.focused().doc.bytes().to_vec();
                        ws.place_focused_cursor(crate::lsp::model::lsp_pos_to_byte(&bytes, l, c));
                    } else {
                        *status = opened.status;
                    }
                }
            }
            if !matches!(outcome, PickOutcome::Continue) {
                *ref_picker = None;
            }
            return;
        }
        // F-014: the code-action picker applies the selected action's WorkspaceEdit (the multi-file apply,
        // inline here since the `spans` borrow is released post-render — like the references jump).
        if let Some(outcome) = action_picker.as_mut().map(|p| p.on_key(key)) {
            if let PickOutcome::Accept = outcome {
                if let Some(action) = action_picker.as_ref().and_then(|p| p.selected().cloned()) {
                    lsp.apply_code_action(&action, ws, files, highlighters, status);
                }
            }
            if !matches!(outcome, PickOutcome::Continue) {
                *action_picker = None;
            }
            return;
        }
        // F-014: the diagnostics picker jumps the cursor to the selected diagnostic's byte offset.
        if let Some(outcome) = diag_picker.as_mut().map(|p| p.on_key(key)) {
            if let PickOutcome::Accept = outcome {
                if let Some(off) = diag_picker.as_ref().and_then(|p| p.selected().copied()) {
                    ws.place_focused_cursor(off);
                }
            }
            if !matches!(outcome, PickOutcome::Continue) {
                *diag_picker = None;
            }
            return;
        }
        // F-029: the `:registers` viewer is view-only — any non-Continue outcome (Enter / Esc) just closes it.
        if let Some(outcome) = reg_picker.as_mut().map(|p| p.on_key(key)) {
            if !matches!(outcome, PickOutcome::Continue) {
                *reg_picker = None;
            }
            return;
        }
        // The `:digraphs` listing is view-only — any non-Continue outcome (Enter / Esc) just closes it.
        if let Some(outcome) = digraph_picker.as_mut().map(|p| p.on_key(key)) {
            if !matches!(outcome, PickOutcome::Continue) {
                *digraph_picker = None;
            }
            return;
        }
        // The `[I`/`]I` keyword-line listing is view-only (Vim only displays) — any non-Continue closes it.
        if let Some(outcome) = keyword_lines_picker.as_mut().map(|p| p.on_key(key)) {
            if !matches!(outcome, PickOutcome::Continue) {
                *keyword_lines_picker = None;
            }
            return;
        }
        // F-003: the `:marks` viewer jumps the cursor to the selected mark's byte offset on Enter.
        if let Some(outcome) = marks_picker.as_mut().map(|p| p.on_key(key)) {
            if let PickOutcome::Accept = outcome {
                if let Some(off) = marks_picker.as_ref().and_then(|p| p.selected().copied()) {
                    ws.place_focused_cursor(off);
                }
            }
            if !matches!(outcome, PickOutcome::Continue) {
                *marks_picker = None;
            }
            return;
        }
        // F-003: the `:jumps` / `:changes` position viewer jumps the cursor to the selected offset on Enter.
        if let Some(outcome) = pos_picker.as_mut().map(|p| p.on_key(key)) {
            if let PickOutcome::Accept = outcome {
                if let Some(off) = pos_picker.as_ref().and_then(|p| p.selected().copied()) {
                    ws.place_focused_cursor(off);
                }
            }
            if !matches!(outcome, PickOutcome::Continue) {
                *pos_picker = None;
            }
            return;
        }
        // The command palette dispatches the selected command by its stable id, through the normal command
        // path (so it undoes/records like any other).
        if let Some(outcome) = palette.as_mut().map(|p| p.on_key(key)) {
            if let PickOutcome::Accept = outcome {
                let cmd = palette.as_ref().and_then(|p| p.selected().cloned());
                if let Some(cmd) = cmd {
                    run_cmd(cmd, ws, files, recorded, status, quit);
                }
            }
            if !matches!(outcome, PickOutcome::Continue) {
                *palette = None;
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
                    Mode::TerminalNormal => {
                        if matches!(key.code, KeyCode::Char('i' | 'a' | 'A')) {
                            run_cmd(Command::EnterTerminal, ws, files, recorded, status, quit);
                            return;
                        }
                        // else: fall through to the normal editor grammar (read-only over an empty doc).
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
                    // A closed fold needs ≥ 2 lines: `end > start` (nvim declines a single-line `zf`).
                    if let Some((start, end)) = ws.reindent_range(motion, count) {
                        if end > start {
                            let buf_id = ws.focused_buffer();
                            let fv = folds.entry(buf_id).or_default();
                            crate::folds::insert_closed_fold(fv, start, end);
                            ws.place_focused_cursor(line_idx.nth_line_start(start));
                            *status = format!("folded {} lines", end - start + 1);
                        }
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
            // Scroll-align verbs (zz/zt/zb/z./z-/z<CR>). `{count}` first moves the cursor to line `count`
            // (clamped to the last line): `zz`/`zt`/`zb` keep its column, `z.`/`z<CR>`/`z-` put it on the
            // first non-blank, with or without a count (`:help z<CR>`). Neither is a jump.
            let to = match key.code {
                KeyCode::Char('z' | '.') => Some(viewport::RecenterTo::Center),
                KeyCode::Char('t') | KeyCode::Enter => Some(viewport::RecenterTo::Top),
                KeyCode::Char('b' | '-') => Some(viewport::RecenterTo::Bottom),
                _ => None,
            };
            if let Some(to) = to {
                let cur = line_idx.line_of(ws.focused().view.cursor());
                let last = line_idx.line_of(snapshot.len().saturating_sub(1));
                let row = if count > 0 {
                    (count as usize - 1).min(last)
                } else {
                    cur
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
                    ws.place_focused_cursor(start + off);
                }
                ws.set_top(ws.focus(), viewport::recenter(row, focused_h, to));
                return;
            }
            // Fold verbs (manual folds; F-007 carve-out). `zf` folds the Visual selection; the rest act on the fold at the cursor.
            let buf = ws.focused_buffer();
            let cur_line = line_idx.line_of(ws.focused().view.cursor());
            let fv = folds.entry(buf).or_default();
            match key.code {
                // `zf` — create a CLOSED fold. From a Visual selection: fold the selection's lines. From
                // Normal mode: arm the `zf{motion}` sub-state (handled at the top of the loop), which folds
                // the range the following motion covers (slice 2).
                KeyCode::Char('f') => {
                    if let Some((s, e)) = ws.focused().view.selection_span(snapshot) {
                        let (a, b) = (line_idx.line_of(s), line_idx.line_of(e.saturating_sub(1)));
                        let (start, end) = (a.min(b), a.max(b));
                        if end > start {
                            crate::folds::insert_closed_fold(fv, start, end);
                            run_cmd(Command::EnterNormal, ws, files, recorded, status, quit);
                            ws.place_focused_cursor(line_idx.nth_line_start(start));
                            *status = format!("folded {} lines", end - start + 1);
                        }
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
                // `zo`/`zc`/`za` — open / close / toggle the fold containing the cursor line.
                KeyCode::Char('o' | 'c' | 'a') => {
                    if let Some(idx) = crate::folds::fold_at(fv, cur_line) {
                        fv[idx].closed = match key.code {
                            KeyCode::Char('o') => false,
                            KeyCode::Char('c') => true,
                            _ => !fv[idx].closed,
                        };
                        let start = fv[idx].start;
                        if fv[idx].closed {
                            ws.place_focused_cursor(line_idx.nth_line_start(start));
                        }
                    }
                }
                // `zj` / `zk` — move the cursor DOWN to the next fold's start / UP to the previous fold's
                // end (`{count}` repeats; a closed fold counts as one, which is inherent since we step over
                // whole folds, not lines). Cursor-only, column 0 — matching nvim, which ignores 'startofline'
                // here (verified: `zj` lands at col 0 even with `startofline` on). Landing inside a closed
                // fold is snapped onto its summary row by the per-frame fold reconcile.
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
                        ws.place_focused_cursor(line_idx.nth_line_start(line));
                    }
                }
                // `zR`/`zM`/`zr`/`zm` — open/close all folds or step the foldlevel. Under
                // `foldmethod=indent` these DRIVE the foldlevel value (Vim: zR=max, zM=0, zr=+1, zm=-1) and
                // re-apply the closed rule to the derived folds; under `manual` they open/close the flat set
                // directly (manual folds are a single level, so zr≡zR and zm≡zM there).
                KeyCode::Char('R' | 'M' | 'r' | 'm') => {
                    let is_indent =
                        fold_method.get(&buf).copied() == Some(crate::input::FoldMethod::Indent);
                    if is_indent {
                        let maxl = crate::folds::max_level(fv);
                        let cur = foldlevel.get(&buf).copied().unwrap_or(0);
                        let new = match key.code {
                            KeyCode::Char('R') => maxl,                // open all
                            KeyCode::Char('M') => 0,                   // close all
                            KeyCode::Char('r') => (cur + 1).min(maxl), // one level more open
                            _ => cur.saturating_sub(1), // 'm' — one level more closed
                        };
                        foldlevel.insert(buf, new);
                        let en = foldenable.get(&buf).copied().unwrap_or(true);
                        crate::folds::apply_foldlevel(fv, if en { new } else { usize::MAX });
                    } else {
                        let close = matches!(key.code, KeyCode::Char('M' | 'm'));
                        fv.iter_mut().for_each(|f| f.closed = close);
                    }
                }
                // `zd` — delete the fold at the cursor line.
                KeyCode::Char('d') => {
                    if let Some(idx) = crate::folds::fold_at(fv, cur_line) {
                        fv.remove(idx);
                    }
                }
                // `zE` — eliminate ALL folds in the window (removes them, unlike `zR` which only opens).
                KeyCode::Char('E') => crate::folds::eliminate_all(fv),
                // `zv` — view cursor line: open just enough folds to reveal the cursor line (for the
                // non-nesting slice-1 model, the single fold containing it). Keeps the fold, unlike `zd`.
                KeyCode::Char('v') => {
                    crate::folds::open_cursor_fold(fv, cur_line);
                }
                _ => {}
            }
            return;
        }
        // F-007 window layer: a `C-w` prefix (Normal mode, no command-line, not Insert where `C-w`
        // deletes a word) arms the next key as a window command. A thin frontend intercept for MVP;
        // F-003's keymap router will absorb it into a proper layer.
        if matches!(prefix, Prefix::Window) {
            *prefix = Prefix::None;
            dispatch_window(key, ws, quit, status);
            return;
        }
        // The command-line window (`q:`/`q/`/`q?`) owns the keystream while open, like the `:` line: suppress
        // every Normal-mode intercept below so its keys (`j`/`k`/`<CR>`/`<Esc>`/`<C-c>`) reach `engine.feed`
        // → `feed_cmdwin` untouched.
        let normal = matches!(ws.focused().view.mode(), Mode::Normal)
            && engine.cmdline().is_none()
            && engine.cmdwin().is_none();
        let visual = matches!(
            ws.focused().view.mode(),
            Mode::Visual { .. } | Mode::Select { .. }
        );
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
        // frontend view move, everything else (`]p`, `[M`, …) stays the engine's. `[z` → the current fold's
        // START, `]z` → its END (Vim's "current open fold"); non-overlapping slice-1 folds make `fold_at` the
        // innermost one. No fold at the cursor → no-op (Vim beeps). Count is ignored (Vim: `[z`/`]z` take
        // none), so consume it via `take_count`, which also resets the engine's `BracketPrefix`.
        if let Pending::Bracket { open } = pending {
            if normal && key.code == KeyCode::Char('z') && key.modifiers.is_empty() {
                engine.take_count();
                let buf = ws.focused_buffer();
                let cur_line = line_idx.line_of(ws.focused().view.cursor());
                if let Some(fv) = folds.get(&buf) {
                    if let Some(idx) = crate::folds::fold_at(fv, cur_line) {
                        let target = if open { fv[idx].start } else { fv[idx].end };
                        ws.place_focused_cursor(line_idx.nth_line_start(target));
                    }
                }
                return;
            }
        }
        let plain_z = key.code == KeyCode::Char('z') && key.modifiers.is_empty();
        let session_chord = plain_z
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
            engine.take_count();
            *prefix = Prefix::Window;
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
            *palette = Some(palette::open(&focused_context(ws), engine));
            return;
        }
        // `C-l` opens the buffer-line fuzzy picker (F-013 NAT-3). Normal-only, so the Emacs profile's
        // non-modal C-l (recenter) is unaffected; C-l is unbound in the Vim/Native Normal grammar.
        if chord && is_ctrl(key, 'l') {
            engine.take_count();
            *line_picker = Some(line_picker::open(ws.focused().doc.bytes()));
            return;
        }
        // `C-b` opens the buffer picker (F-013 NAT-3). Normal-only, so the Emacs profile's non-modal C-b
        // (backward-char) is unaffected; C-b is unbound in the Vim/Native Normal grammar (no page-scroll).
        if chord && is_ctrl(key, 'b') {
            engine.take_count();
            *buffer_picker = Some(buffer_picker::open(ws));
            return;
        }
        // `C-f` opens the fuzzy file finder (F-013 NAT-3). Normal-only, so the Emacs non-modal C-f
        // (forward-char) is unaffected; in the Vim/Native Normal grammar the plain `f` find-char has no
        // ctrl, so intercepting ctrl-f shadows nothing reachable.
        if chord && is_ctrl(key, 'f') {
            engine.take_count();
            *file_picker = Some(file_picker::open());
            return;
        }
        // `z` arms the scroll/fold prefix with the count typed before it. In Normal (scroll + fold verbs) and
        // Visual (so `zf` folds the selection); plain `z` (no ctrl), never on the command line.
        if (chord || (visual && pending == Pending::Base)) && plain_z {
            *prefix = Prefix::Z(engine.take_count());
            return;
        }
        // `&` repeats the last `:s` on the CURRENT line, keeping its flags: nvim's default mapping is
        // `nnoremap & :&&<CR>`, so `{count}&` is `:.,.+{count-1}&&`. A range past the last line is E16, as
        // there. No-op before any `:s`.
        if chord && key.code == KeyCode::Char('&') && key.modifiers.is_empty() {
            let count = engine.take_count().max(1) as usize;
            let bytes = ws.focused().doc.bytes();
            let first = ruse_core::pos::line_of(bytes, ws.focused().view.cursor()) + 1;
            let lines = bytes.split(|b| *b == b'\n').count() - usize::from(bytes.ends_with(b"\n"));
            let last = first + count - 1;
            *status = match &last_substitute {
                Some(_) if last > lines.max(1) => "E16: Invalid range".to_string(),
                Some((pat, rep, flags)) => {
                    match ws.substitute(ruse_core::SubRange::Lines(first, last), pat, rep, *flags) {
                        Ok(out) if out.replacements == 0 => {
                            format!("E486: pattern not found: {pat}")
                        }
                        Ok(out) => {
                            format!("{} substitutions on {} lines", out.replacements, out.lines)
                        }
                        Err(e) => crate::app::dispatch::regex_error_msg(&e),
                    }
                }
                None => "no previous substitute".to_string(),
            };
            return;
        }
        // `C-d` / `C-u` scroll a half page: move the cursor half the pane down / up (column preserved via
        // the core Move), and the per-frame scroll pass follows it. `C-f`/`C-b` are taken by the pickers.
        if chord && (is_ctrl(key, 'd') || is_ctrl(key, 'u')) {
            engine.take_count(); // consume any pending count so it can't leak onto the next command
            let half = (focused_h / 2).max(1) as u32;
            let m = if is_ctrl(key, 'd') {
                ruse_core::Motion::Down
            } else {
                ruse_core::Motion::Up
            };
            run_cmd(Command::Move(half, m), ws, files, recorded, status, quit);
            return;
        }
        // `C-e` / `C-y` scroll the view one line down / up, nudging the cursor only if it would leave the
        // scrolloff band. Frontend-only (viewport concern); `{count}` scrolls that many lines.
        if chord && (is_ctrl(key, 'e') || is_ctrl(key, 'y')) {
            let n = engine.take_count().max(1) as usize;
            let last_line = line_idx.line_of(snapshot.len());
            let cursor_row = line_idx.line_of(ws.focused().view.cursor());
            let (nt, nr) = viewport::scroll_lines(
                ws.focused().view.top(),
                cursor_row,
                focused_h,
                SCROLLOFF,
                n,
                is_ctrl(key, 'e'),
                last_line,
            );
            ws.set_top(ws.focus(), nt);
            if nr != cursor_row {
                run_cmd(
                    Command::Move(nr as u32 + 1, ruse_core::Motion::GotoLine),
                    ws,
                    files,
                    recorded,
                    status,
                    quit,
                );
            }
            return;
        }
        // `i_CTRL-E` / `i_CTRL-Y` (Insert mode) — insert the char directly BELOW / ABOVE the caret at the
        // same column. The engine has no buffer, so resolve the adjacent-line char here and hand it to the
        // engine, which emits (and dot-records) a concrete `InsertChar`. `None` (short/absent adjacent
        // line) is a Vim no-op. Guarded by `insert_plain_text_ctx` so a pending Insert prefix (e.g.
        // `CTRL-R`'s register name) still owns the key. Normal-mode `C-e`/`C-y` scroll is handled above.
        if matches!(ws.focused().view.mode(), Mode::Insert)
            && (is_ctrl(key, 'e') || is_ctrl(key, 'y'))
            && engine.insert_plain_text_ctx()
        {
            let ch = ws.adjacent_line_char(is_ctrl(key, 'y'));
            if let Feed::Cmd(cmd) = engine.insert_copy_char(ch) {
                run_cmd(cmd, ws, files, recorded, status, quit);
            }
            return;
        }
        // `i_CTRL-N` / `i_CTRL-P` (Insert mode) — keyword completion from the CURRENT buffer (F-003). The
        // engine has no buffer, so on the FIRST key the frontend resolves the base + candidate set (nvim
        // scan order) and hands them to the engine, which caches them and cycles the selection; subsequent
        // keys just step the cached cycle. The engine emits (and dot-records) a `CompleteWord` swapping the
        // completion region. Guarded by `insert_plain_text_ctx` (a pending Insert prefix still owns its key).
        // Other completion sources / the popup menu are out of scope for this slice (deferred).
        if matches!(ws.focused().view.mode(), Mode::Insert)
            && (is_ctrl(key, 'n') || is_ctrl(key, 'p'))
            && engine.insert_plain_text_ctx()
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
        if matches!(ws.focused().view.mode(), Mode::Insert) && is_ctrl(key, 'l') {
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
            let last_line = line_idx.line_of(snapshot.len());
            // Under an operator Vim IGNORES 'scrolloff' — `dH`/`dL` operate through the TRUE top/bottom visible
            // line (verified against nvim), whereas the bare cursor-motion keeps the scrolloff margin.
            let scrolloff = if engine.has_op() { 0 } else { SCROLLOFF };
            let row = viewport::screen_line(
                ws.focused().view.top(),
                focused_h,
                scrolloff,
                to,
                count,
                last_line,
            );
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
        match engine.feed(key, ws.focused().view.mode()) {
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
            Feed::Pending | Feed::Ignored => {}
            Feed::Cmd(cmd) => {
                // `g&` — repeat the last `:s` over the WHOLE FILE with its flags. Resolved here (the engine
                // has no substitute history); like the current-line `&`, it calls `substitute` directly.
                if matches!(cmd, Command::RepeatSubstituteGlobal) {
                    *status = match &last_substitute {
                        Some((pat, rep, flags)) => {
                            match ws.substitute(ruse_core::SubRange::WholeFile, pat, rep, *flags) {
                                Ok(out) if out.replacements == 0 => {
                                    format!("E486: pattern not found: {pat}")
                                }
                                Ok(out) => {
                                    format!(
                                        "{} substitutions on {} lines",
                                        out.replacements, out.lines
                                    )
                                }
                                Err(e) => crate::app::dispatch::regex_error_msg(&e),
                            }
                        }
                        None => "no previous substitute".to_string(),
                    };
                    return;
                }
                // `&` — repeat the last `:s` on the CURRENT LINE, DROPPING its flags (Vim: no `g`/`i`/`I`,
                // so only the first match on the cursor's line is replaced). Same frontend resolution as
                // `g&`, but scoped to the current line and forced to default flags.
                if matches!(cmd, Command::RepeatSubstituteLine) {
                    *status = match &last_substitute {
                        Some((pat, rep, _flags)) => {
                            match ws.substitute(
                                ruse_core::SubRange::CurrentLine,
                                pat,
                                rep,
                                ruse_core::SubFlags::default(),
                            ) {
                                Ok(out) if out.replacements == 0 => {
                                    format!("E486: pattern not found: {pat}")
                                }
                                Ok(out) => {
                                    format!(
                                        "{} substitutions on {} lines",
                                        out.replacements, out.lines
                                    )
                                }
                                Err(e) => crate::app::dispatch::regex_error_msg(&e),
                            }
                        }
                        None => "no previous substitute".to_string(),
                    };
                    return;
                }
                // INFO commands (`ga`/`:ascii`, `CTRL-G`, `g CTRL-G`): print to the status line, no buffer
                // mutation. Like `*`/`gd`, the engine has no buffer, so the frontend reads the focused
                // buffer's bytes + cursor here and formats the message via the pure `ruse_core::info` helpers.
                if let Some(msg) = info_status(&cmd, ws) {
                    *status = msg;
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
                            let cursor_line =
                                ruse_core::pos::line_of(bytes, pane.view.cursor()) + 1;
                            let matches = ruse_core::keyword_line_numbers(bytes, &keyword);
                            if list {
                                let rows: Vec<(usize, String)> =
                                    ruse_core::keyword_list(&matches, cursor_line, above)
                                        .into_iter()
                                        .map(|l| (l, ruse_core::line_text(bytes, l)))
                                        .collect();
                                *status = format!("{} line(s) matching \"{keyword}\"", rows.len());
                                *keyword_lines_picker =
                                    Some(crate::ui::keyword_lines_picker::open(&rows));
                            } else {
                                *status = match ruse_core::keyword_echo(
                                    &matches,
                                    cursor_line,
                                    above,
                                    count as usize,
                                ) {
                                    ruse_core::KeywordEcho::Line(l) => {
                                        ruse_core::line_text(bytes, l)
                                    }
                                    ruse_core::KeywordEcho::CurrentLine => {
                                        "E387: Match is on current line".into()
                                    }
                                    ruse_core::KeywordEcho::NotFound => {
                                        "E389: Couldn't find pattern".into()
                                    }
                                };
                            }
                        }
                        None => *status = "E349: No identifier under cursor".into(),
                    }
                    return;
                }
                // `*`/`#` (word under cursor): the engine has no buffer, so resolve the keyword here, then
                // rewrite to a concrete search — records the deterministic pattern and drives hlsearch/`n`.
                let cmd = if let Command::SearchWordUnder {
                    forward,
                    whole_word,
                } = cmd
                {
                    match ws.word_under_cursor() {
                        Some(word) => {
                            // Keyword chars (alnum/`_`/non-ASCII) are regex-safe; `\<…\>` = whole word.
                            let pat = if whole_word {
                                format!("\\<{word}\\>")
                            } else {
                                word
                            };
                            // Record the direction too, so a following `n`/`N` repeats relative to the
                            // `*`/`#` that set it (`#` then `n` continues backward).
                            engine.set_last_search(pat.clone(), forward);
                            if forward {
                                Command::SearchNext(pat)
                            } else {
                                Command::SearchPrev(pat)
                            }
                        }
                        None => {
                            *status = "E348: No string under cursor".into();
                            return;
                        }
                    }
                } else {
                    cmd
                };
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
                            *status = "E348: No string under cursor".into();
                            return;
                        }
                    }
                } else {
                    cmd
                };
                // Tree-aware `=` (F-015): if the focused buffer has a live syntax tree, resolve the reindent
                let cmd = resolve_tree_aware(cmd, ws, highlighters, line_idx, snapshot);
                // A completed search turns on hlsearch for that pattern (F-009 #1).
                if let Some(p) = search_pattern(&cmd) {
                    *search_hl = Some(p);
                }
                // Refresh the read-only special registers `"/ ": ". "%` before dispatch so a `"/p` /
                // `C-r :` / `".p` / `"%p` in this command reads the live values (`:help quote_/`).
                sync_special_registers(ws, engine, last_ex_line, files);
                run_cmd(cmd, ws, files, recorded, status, quit);
            }
            // `.` (dot-repeat) replays the last change; record and apply each concrete command so the
            // trace (F-022) captures the resolved edit, not the `.` keypress.
            Feed::Replay(cmds) => {
                sync_special_registers(ws, engine, last_ex_line, files);
                for cmd in cmds {
                    run_cmd(cmd, ws, files, recorded, status, quit);
                }
            }
        }
    }

    /// Run a finished `:`-line (F-026): the Ex commands that need session state beyond `run_ex`'s.
    fn execute_ex(&mut self, text: String, frame: &FrameCtx) {
        let Session {
            ws,
            files,
            highlighters,
            initial,
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
            search_hl,
            hlsearch_on,
            incsearch_on,
            fixeol_on,
            last_substitute,
            last_ex_line,
            diag_picker,
            reg_picker,
            digraph_picker,
            marks_picker,
            pos_picker,
            #[cfg(unix)]
            terminals,
            lsp,
            status,
            ..
        } = self;
        let (cols, text_rows) = (frame.cols, frame.text_rows);
        let (snapshot, rects) = (&frame.snapshot, &frame.rects);
        // Record the executed command-line for Vim's `":` register (`:help quote_:`), without the
        // leading `:` (the engine already strips it). Read by `C-r :` / `":p`.
        *last_ex_line = Some(text.clone());
        match parse_ex(&text) {
            Ex::NoHighlight => *search_hl = None, // `:noh` clears the search highlight (F-009 #1)
            // `:set (no)hlsearch/incsearch` — frontend render toggles (behavior stays on by default).
            Ex::SetHlSearch(on) => {
                *hlsearch_on = on;
                if !on {
                    *search_hl = None; // turning hlsearch off also clears any live highlight (Vim)
                }
                *status = format!("hlsearch {}", if on { "on" } else { "off" });
            }
            Ex::SetIncSearch(on) => {
                *incsearch_on = on;
                *status = format!("incsearch {}", if on { "on" } else { "off" });
            }
            // `:set (no)fixeol` — opt-in trailing-newline-on-save (frontend write flag; OFF default).
            Ex::SetFixEol(on) => {
                *fixeol_on = on;
                *status = format!("fixeol {}", if on { "on" } else { "off" });
            }
            // `:set foldmethod=indent|manual` (frontend fold state, slice 3). Switching just flips the
            // method + forces a recompute next frame; existing folds are kept (Vim keeps them).
            Ex::SetFoldMethod(m) => {
                let buf = ws.focused_buffer();
                fold_method.insert(buf, m);
                fold_rev.remove(&buf); // recompute (or leave manual folds) on the next frame
                *status = match m {
                    crate::input::FoldMethod::Indent => "foldmethod=indent".into(),
                    crate::input::FoldMethod::Manual => "foldmethod=manual".into(),
                };
            }
            // `:set foldlevel=N` — folds deeper than N close under indent method; re-apply now.
            Ex::SetFoldLevel(n) => {
                let buf = ws.focused_buffer();
                foldlevel.insert(buf, n);
                if fold_method.get(&buf).copied() == Some(crate::input::FoldMethod::Indent) {
                    if let Some(fv) = folds.get_mut(&buf) {
                        let en = foldenable.get(&buf).copied().unwrap_or(true);
                        crate::folds::apply_foldlevel(fv, if en { n } else { usize::MAX });
                    }
                }
                *status = format!("foldlevel={n}");
            }
            // `:set (no)foldenable` — master switch; off opens everything under the indent method.
            Ex::SetFoldEnable(on) => {
                let buf = ws.focused_buffer();
                foldenable.insert(buf, on);
                if fold_method.get(&buf).copied() == Some(crate::input::FoldMethod::Indent) {
                    if let Some(fv) = folds.get_mut(&buf) {
                        let fdl = if on {
                            foldlevel.get(&buf).copied().unwrap_or(0)
                        } else {
                            usize::MAX
                        };
                        crate::folds::apply_foldlevel(fv, fdl);
                    }
                }
                *status = format!("foldenable {}", if on { "on" } else { "off" });
            }
            // `:fmt` / `:rename {new}` / `:references` / `:codeaction` (F-014): the coordinator sends
            // the request; the response is dispatched + applied (or opens a picker) on a later frame.
            ex @ (Ex::Format | Ex::Rename(_) | Ex::References | Ex::CodeAction) => {
                lsp.on_ex(&ex, ws, files, snapshot, status);
            }
            // `:trust` (D-058): the user grants workspace trust for this session; language servers
            // start on the next frame's sync.
            Ex::Trust => lsp.grant_trust(status),
            // `:diagnostics` (F-014): open a picker over the focused buffer's already-collected
            // diagnostics (no server round-trip); Enter jumps to the selected one.
            Ex::Diagnostics => {
                let diags = lsp.diagnostics_for(ws.focused_buffer());
                if diags.is_empty() {
                    *status = "no diagnostics".to_string();
                } else {
                    *status = format!("{} diagnostic(s)", diags.len());
                    *diag_picker = Some(diag_picker::open(diags, snapshot));
                }
            }
            // `:registers` (F-029): open a view-only picker over the non-empty registers, so a
            // recorded macro (`"a`) or a yank is inspectable.
            Ex::Registers => {
                let snapshot = ws.register_snapshot();
                if snapshot.is_empty() {
                    *status = "no registers set".to_string();
                } else {
                    *status = format!("{} register(s)", snapshot.len());
                    *reg_picker = Some(register_picker::open(snapshot));
                }
            }
            // `:ascii`/`:as` — the ex synonym of `ga`: print the char-under-cursor's value.
            Ex::Ascii => {
                let pane = ws.focused();
                *status = ruse_core::ascii_info(pane.doc.bytes(), pane.view.cursor());
            }
            // `:digraphs` / `:dig`: open the view-only digraph-listing overlay.
            Ex::Digraphs => {
                let p = crate::ui::digraph_picker::open();
                *status = format!("{} digraph(s)", p.rows().len());
                *digraph_picker = Some(p);
            }
            // `:marks` (F-003): open a picker over the set marks; Enter jumps to the selected one.
            Ex::Marks => {
                let snapshot = ws.marks_snapshot();
                if snapshot.is_empty() {
                    *status = "no marks set".to_string();
                } else {
                    *status = format!("{} mark(s)", snapshot.len());
                    *marks_picker = Some(marks_picker::open(snapshot, ws.focused().doc.bytes()));
                }
            }
            // `:jumps` / `:changes` (F-003): position viewers over the jumplist / change list;
            // Enter jumps. Both share the `pos_picker` overlay (never open at once).
            Ex::Jumps | Ex::Changes => {
                let positions = if matches!(parse_ex(&text), Ex::Jumps) {
                    ws.jumps_snapshot()
                } else {
                    ws.changes_snapshot()
                };
                if positions.is_empty() {
                    *status = "empty".to_string();
                } else {
                    *status = format!("{} position(s)", positions.len());
                    *pos_picker = Some(pos_picker::open(positions, ws.focused().doc.bytes()));
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
                            *status = "terminal started (CTRL-\\ CTRL-N to leave)".to_string();
                        }
                        Err(e) => *status = format!("E: cannot start terminal: {e}"),
                    }
                }
                #[cfg(not(unix))]
                {
                    *status = "terminal is not supported on this platform".to_string();
                }
            }
            // Lang-Arg map maintenance (F-027): the map is engine state, so it is applied here
            // where `engine` is in scope, not in `run_ex` (which owns only workspace/file state).
            Ex::Lmap { lhs, rhs } => engine.set_lang_mapping(lhs, rhs),
            Ex::Lunmap { lhs } => engine.clear_lang_mapping(lhs),
            // `:[range]normal[!] {keys}` replays the key payload through the input `engine` (in scope
            // here, not in `run_ex`) → the same `Command` pipeline as typed keys / macro replay.
            Ex::Normal { range, keys, .. } => {
                crate::app::dispatch::run_normal(
                    engine, ws, range, &keys, files, recorded, status, quit,
                );
            }
            // `:g/pat/normal[!] {keys}` — a `:g` whose payload replays keys per marked line, so it
            // needs the input `engine` (in scope here, not in `run_ex`). The `d`/`s` payloads have no
            // engine dependency and fall through to `run_ex`'s core two-pass below.
            Ex::Global(ref spec)
                if matches!(spec.cmd, crate::input::GlobalPayload::Normal { .. }) =>
            {
                if let crate::input::GlobalPayload::Normal { keys, .. } = &spec.cmd {
                    crate::app::dispatch::run_global_normal(
                        engine,
                        ws,
                        spec.range,
                        &spec.pattern,
                        spec.negate,
                        keys,
                        files,
                        recorded,
                        status,
                        quit,
                    );
                }
            }
            // `:checkhealth` (F-030): gather the frontend snapshot HERE (guard/profile/highlighter
            // are in scope, not in `run_ex`) and render the report's one-line summary into status.
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
                    file_ext: files
                        .get(&ws.focused_buffer())
                        .and_then(|bf| bf.path.extension())
                        .map(|e| e.to_string_lossy().into_owned()),
                    grammar_ok: highlighters.contains_key(&ws.focused_buffer()),
                    buffers: ws.window_count(),
                    trace_commands: recorded.len(),
                };
                *status = health::summary_line(&health::report(&inputs));
            }
            // `:earlier`/`:later` walk chronological undo time by recording + applying N of the
            // g-/g+ commands (done HERE so they land in `recorded` for trace fidelity).
            Ex::Earlier(n) | Ex::Later(n) => {
                // The pattern binds `n` but not the direction; recover it from the parsed command.
                let older = matches!(parse_ex(&text), Ex::Earlier(_));
                let cmd = if older {
                    Command::UndoOlder
                } else {
                    Command::UndoNewer
                };
                for _ in 0..n {
                    run_cmd(cmd.clone(), ws, files, recorded, status, quit);
                }
                *status = format!("{n} change(s) {}", if older { "earlier" } else { "later" });
            }
            // `:e {file}` opens a file into a new buffer (F-007) — shared with the file picker.
            Ex::Edit(file) => {
                *status = open_file_into_buffer(&file, ws, files, highlighters).status;
            }
            // `:e!` reloads the focused buffer's file from disk, discarding unsaved changes.
            Ex::EditReload => {
                let id = ws.focused_buffer();
                match files.get(&id).map(|bf| bf.path.clone()) {
                    Some(path) => match persist::read_file(&path) {
                        Ok(raw) => {
                            let fmt = persist::encoding::FileFormat::detect(&raw);
                            ws.reload_focused(fmt.to_buffer(&raw));
                            files.insert(
                                id,
                                BufferFile {
                                    path: path.clone(),
                                    fmt,
                                },
                            );
                            *status = format!("\"{}\" reloaded", path.display());
                        }
                        Err(e) => {
                            *status = format!("E484: can't open {}: {e}", path.display());
                        }
                    },
                    None => *status = "E32: No file name".into(),
                }
            }
            // `:[addr]r {file}` / `:[addr]r !{cmd}` — read a file's contents (pure IO) or a shell
            // command's stdout below the addressed line. Done HERE (not in `run_ex`) because the read
            // is IMPURE (fs / process); core only splices the resulting bytes. The cursor lands on the
            // FIRST inserted line for a file read, the LAST for a command read (nvim v0.12.4 quirk).
            Ex::Read { addr, source } => match source {
                ReadSource::File(path) => match persist::read_file(std::path::Path::new(&path)) {
                    Ok(raw) => {
                        // Strip the read file's own BOM / CRLF as opening it would; the target
                        // buffer's format is applied on save (#860).
                        let clean = persist::encoding::FileFormat::normalize(&raw);
                        let n = ws.read_lines(addr, &clean, false);
                        *status = format!("\"{path}\" {n}L, {}B", raw.len());
                    }
                    Err(e) => *status = format!("E484: can't open {path}: {e}"),
                },
                ReadSource::Command(cmd) => match crate::shell::capture(&cmd) {
                    Ok(out) => {
                        let n = ws.read_lines(addr, out.text.as_bytes(), true);
                        let lines = if n == 1 {
                            "1 more line".to_string()
                        } else {
                            format!("{n} more lines")
                        };
                        *status = with_shell_failure(&out, lines);
                    }
                    Err(e) => *status = e.to_string(),
                },
            },
            // `:{range}!{cmd}` — FILTER: pipe the range's lines through the shell command and replace
            // them with its output. Pull the range text out FIRST (read-only), shell out, then splice.
            // As in nvim the output (stdout + stderr) replaces the range even when the command fails,
            // and the status line says `shell returned {n}` (#861).
            Ex::Filter { range, cmd } => match ws.range_text(range) {
                Some(input) => match crate::shell::filter(&cmd, &input) {
                    Ok(out) => {
                        let n = ws.filter_lines(range, out.text.as_bytes());
                        let lines = if n <= 1 {
                            String::new()
                        } else {
                            format!("{n} lines filtered")
                        };
                        *status = with_shell_failure(&out, lines);
                    }
                    Err(e) => *status = e.to_string(),
                },
                None => *status = "E749: empty buffer".to_string(),
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
                    *status = match out.failure() {
                        Some(note) => format!("{shown} | {note}"),
                        None => shown,
                    };
                }
                Err(e) => *status = e.to_string(),
            },
            // `:bd` deletes the focused buffer — done HERE so the buffer's `files`/highlighter
            // entries are dropped (both are `&mut` in scope, not in `run_ex`). Guards unsaved
            // changes with E89 unless `!` forces it.
            Ex::BufferDelete { force } => {
                let id = ws.focused_buffer();
                if !force && ws.focused().doc.is_modified() {
                    *status = "E89: No write since last change (add ! to override)".into();
                } else {
                    ws.remove_buffer(id);
                    files.remove(&id);
                    highlighters.remove(&id);
                    *status = format!("buffer {} deleted", id.0);
                }
            }
            // Bare `:s` / `:s {flags}` / `:&` / `:&&` — repeat the last `:s` against the frontend's
            // substitute history (the core keeps none), exactly like `&`/`g&`. `flags = None` (`:&&`)
            // KEEPS the stored flags; `flags = Some(f)` REPLACES them (bare `:s`/`:&` = default flags,
            // `:s g` = the given flags). Verified vs nvim v0.12.4.
            Ex::RepeatSubstitute { range, flags } => {
                *status = match &last_substitute {
                    Some((pat, rep, stored)) => {
                        let use_flags = flags.unwrap_or(*stored);
                        match ws.substitute(range, pat, rep, use_flags) {
                            Ok(out) if out.replacements == 0 => {
                                format!("E486: pattern not found: {pat}")
                            }
                            Ok(out) => {
                                format!("{} substitutions on {} lines", out.replacements, out.lines)
                            }
                            Err(e) => crate::app::dispatch::regex_error_msg(&e),
                        }
                    }
                    None => "no previous substitute".to_string(),
                };
            }
            mut ex => {
                // An empty `:s` pattern reuses the last search (`/`/`*`), exactly as Vim does.
                crate::input::reuse_last_search(&mut ex, engine.last_search());
                // Record a `:s` so `&` can repeat it (Vim). Only a non-empty pattern is worth
                // remembering (an empty one that could not be filled has nothing to repeat).
                if let Ex::Substitute(spec) = &ex {
                    if !spec.pattern.is_empty() {
                        *last_substitute = Some((
                            spec.pattern.clone(),
                            spec.replacement.clone(),
                            ruse_core::SubFlags {
                                global: spec.global,
                                ignore_case: spec.ignore_case,
                            },
                        ));
                    }
                }
                run_ex(
                    &ex, ws, files, initial, recorded, status, quit, confirm, *fixeol_on,
                )
            }
        }
    }
}

pub(crate) fn run(path: Option<PathBuf>, raw: Vec<u8>) -> io::Result<()> {
    // F-008: detect the original encoding/line-ending once, edit in clean LF, restore it on save.
    let fmt = persist::encoding::FileFormat::detect(&raw);
    let disk = fmt.to_buffer(&raw);
    // A crash may have left an append-only journal of unsaved work; offer it, never auto-apply it.
    // A journal that exists but cannot be read is reported on the status line, never silently skipped (#897).
    let (recovered, journal_error) = match persist::journal::replay_checked(path.as_deref()) {
        Ok(r) => (r, None),
        Err(e) => (
            None,
            Some(persist::journal::unreadable_note(path.as_deref(), &e)),
        ),
    };
    let recovery = persist::assess_recovery(&disk, recovered.as_deref());
    let stale = path.as_deref().is_some_and(persist::journal::is_stale);

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

    // Open-time crash recovery (F-008 #3/#4): the user picks; the original file is never touched.
    let stale_note = stale.then_some(persist::journal::STALE_NOTE);
    let (initial, status) = match recovery {
        persist::Recovery::Available(rec) => {
            match prompt_recovery(&mut out, "q: quit and keep them", stale_note)? {
                RecoveryChoice::Recover => (
                    rec,
                    String::from("ruse — recovered unsaved changes (:w to save)"),
                ),
                RecoveryChoice::Discard => {
                    persist::journal::clear(path.as_deref()); // discarded on the user's say-so
                    (
                        disk,
                        String::from("ruse — recovery discarded; opened disk version"),
                    )
                }
                RecoveryChoice::Quit => return Ok(()), // the journal stays for the next start
            }
        }
        persist::Recovery::None => match journal_error {
            Some(note) => (disk, format!("ruse — {note}")),
            None => (disk, String::from("ruse — :q to quit")),
        },
    };

    // Profile selection (F-012 / F-013): no config loader exists yet, so `RUSE_PROFILE` picks the profile
    // (the same env-override seam as the terminal caps). `emacs` = Emacs, `native` = Native, else Vim.
    let profile = match std::env::var("RUSE_PROFILE").as_deref() {
        Ok("emacs") => "emacs",
        Ok("native") => "native",
        _ => "vim",
    };
    // INV-TRUST-1 / D-058: language servers execute workspace code (build scripts, proc-macros), so they
    // start only once the user trusts the workspace (`RUSE_TRUSTED_WORKSPACES` at startup, or `:trust`).
    let cwd = std::env::current_dir().unwrap_or_default();
    let trusted = crate::trust::workspace_trusted(&cwd);
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
        },
        cwd,
        trusted,
        rescue: true,
    });
    // `"+`/`"*` (`:help quoteplus`): inject the real OS-clipboard provider (shell-out to pbcopy/wl-copy/
    // xclip/clip, resolved once). Degrades to a no-op when no clipboard tool is on PATH, so a headless run
    // still works — `"+p` just pastes nothing.
    s.ws.set_clipboard(Box::new(crate::clipboard::SystemClipboard::detect()));

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

    while !s.quit {
        // SIGTERM / SIGHUP (#857): end the session; the caller journals every modified buffer and the
        // guard restores the terminal. The handler's wake-up lands here via a resize event.
        if crate::signals::pending().is_some() {
            break;
        }
        let (cols, term_rows) = ct_terminal::size().unwrap_or((80, 24));
        let frame = s.pre_frame(cols, term_rows);
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
                .map(|s| vec![(s.start, s.end)])
                .unwrap_or_default()
        } else {
            let active = match s.engine.cmdline() {
                // Incsearch: the pattern being typed, only when `incsearch` is on.
                Some(('/', buf, _)) | Some(('?', buf, _)) if s.incsearch_on && !buf.is_empty() => {
                    Some(buf.to_string())
                }
                // Hlsearch: the last completed search, only when `hlsearch` is on.
                _ if s.hlsearch_on => s.search_hl.clone(),
                _ => None,
            };
            active
                .map(|p| {
                    cached_search
                        .spans(revision, snapshot, visible.clone(), &p)
                        .to_vec()
                })
                .unwrap_or_default()
        };
        // Overlay match rows painted above the status line — at most one picker is open at a time, so all
        // three share the same uniform `rows()` paint slot (F-004 / F-013 NAT-3).
        let overlay_rows: Vec<(String, bool)> = if let Some(p) = s.palette.as_ref() {
            p.rows()
        } else if let Some(p) = s.line_picker.as_ref() {
            p.rows()
        } else if let Some(p) = s.buffer_picker.as_ref() {
            p.rows()
        } else if let Some(p) = s.file_picker.as_ref() {
            p.rows()
        } else if let Some(p) = s.ref_picker.as_ref() {
            p.rows()
        } else if let Some(p) = s.action_picker.as_ref() {
            p.rows()
        } else if let Some(p) = s.diag_picker.as_ref() {
            p.rows()
        } else if let Some(p) = s.reg_picker.as_ref() {
            p.rows()
        } else if let Some(p) = s.digraph_picker.as_ref() {
            p.rows()
        } else if let Some(p) = s.keyword_lines_picker.as_ref() {
            p.rows()
        } else if let Some(p) = s.marks_picker.as_ref() {
            p.rows()
        } else if let Some(p) = s.pos_picker.as_ref() {
            p.rows()
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
        let cmd_line: Option<(char, &str)> = if let Some(p) = s.palette.as_ref() {
            Some(('>', p.query.as_str())) // command palette prompt
        } else if let Some(p) = s.line_picker.as_ref() {
            Some(('#', p.query.as_str())) // line-picker prompt (# = line jump)
        } else if let Some(p) = s.buffer_picker.as_ref() {
            Some(('@', p.query.as_str())) // buffer-picker prompt
        } else if let Some(p) = s.file_picker.as_ref() {
            Some(('~', p.query.as_str())) // file-picker prompt (~ = find file)
        } else if let Some(p) = s.ref_picker.as_ref() {
            Some(('*', p.query.as_str())) // references-picker prompt (* = references to symbol)
        } else if let Some(p) = s.action_picker.as_ref() {
            Some(('!', p.query.as_str())) // code-action-picker prompt (! = actions/fixes at cursor)
        } else if let Some(p) = s.diag_picker.as_ref() {
            Some(('✗', p.query.as_str())) // diagnostics-picker prompt
        } else if let Some(p) = s.reg_picker.as_ref() {
            Some(('"', p.query.as_str())) // registers-viewer prompt (" = registers)
        } else if let Some(p) = s.digraph_picker.as_ref() {
            Some(('§', p.query.as_str())) // digraph-listing prompt (§ = digraphs)
        } else if let Some(p) = s.keyword_lines_picker.as_ref() {
            Some(('i', p.query.as_str())) // keyword-line listing prompt (i = [I/]I identifiers)
        } else if let Some(p) = s.marks_picker.as_ref() {
            Some(('\'', p.query.as_str())) // marks-viewer prompt (' = marks)
        } else if let Some(p) = s.pos_picker.as_ref() {
            Some(('↕', p.query.as_str())) // jumps/changes position-viewer prompt
        } else if let Some(h) = leader_hint.as_ref() {
            Some((' ', h.as_str()))
        } else if let Some(kind) = s.engine.cmdwin() {
            // The command-line window's prompt glyph (`:`/`/`/`?`) with a short hint that `<CR>` runs the line.
            Some((kind, "[cmdwin] <CR> run  <Esc> close"))
        } else {
            s.engine.cmdline().map(|(pfx, t, _)| (pfx, t))
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
        let images = render(
            &mut out,
            &s.ws,
            cmd_line,
            &s.status,
            spans,
            virt_lines,
            rects,
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
        s.post_render(&frame);
        // Keys the session queued come before the terminal: the key half of a split Alt chord (#896), then
        // macro replay (D-055), so `@{reg}`'s decoded keys flow through the same dispatch as if re-typed. A
        // key from the replay queue is `from_replay`.
        let (raw_key, from_replay) = match s.next_queued() {
            Some(queued) => queued,
            None => {
                let k = if let Some(ms) = s.poll_ms() {
                    if event::poll(std::time::Duration::from_millis(ms))? {
                        match event::read()? {
                            Event::Key(k) => k,
                            _ => continue,
                        }
                    } else {
                        continue; // timed out: re-render freshly-drained async output
                    }
                } else {
                    match event::read()? {
                        Event::Key(k) => k,
                        _ => continue,
                    }
                };
                (k, false)
            }
        };
        s.on_key(raw_key, from_replay, &frame);
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

/// Resolve a Normal-mode INFO command (`ga`/`:ascii` → [`Command::AsciiInfo`], `CTRL-G` →
/// [`Command::FileInfo`], `g CTRL-G` → [`Command::CursorInfo`]) into its status-line message against the
/// focused buffer, or `None` for any other command. These print only — the buffer is never mutated — so the
/// run loop just sets `status` and skips dispatch. Splitting this out keeps the whole
/// keystroke→Command→message path unit-testable without a session.
fn info_status(cmd: &Command, ws: &Workspace) -> Option<String> {
    match cmd {
        Command::AsciiInfo => {
            let pane = ws.focused();
            Some(ruse_core::ascii_info(pane.doc.bytes(), pane.view.cursor()))
        }
        Command::FileInfo => {
            let id = ws.focused_buffer();
            let name = ws.buffer_name(id);
            let pane = ws.focused();
            Some(ruse_core::file_info(
                name,
                pane.doc.is_modified(),
                pane.doc.bytes(),
                pane.view.cursor(),
            ))
        }
        Command::CursorInfo => {
            let pane = ws.focused();
            // ruse renders tabs at `render::TAB_WIDTH` columns (not nvim's 8), so the `Col` vcol reflects
            // the on-screen layout the user actually sees.
            Some(ruse_core::cursor_pos_info(
                pane.doc.bytes(),
                pane.view.cursor(),
                crate::ui::render::TAB_WIDTH as usize,
            ))
        }
        _ => None,
    }
}

/// Sync the four read-only special registers `"/ ": ". "%` of the focused view from the current frontend
/// state (`:help quote_/`), so a `"/p` / `C-r :` / `".p` / `"%p` dispatched next reads the live values:
/// `"/` from the search ring, `":` from the last executed Ex line, `".` from the last insert session, and
/// `"%` from the focused buffer's on-disk path (empty for an unnamed buffer). Called before dispatch.
fn sync_special_registers(
    ws: &mut Workspace,
    engine: &InputEngine,
    last_ex_line: &Option<String>,
    files: &Files,
) {
    // `"%` shows the file name as it was opened (a relative path, matching Vim's default `%`); an unnamed
    // buffer has no `Files` entry, so the slot stays empty.
    let file = files
        .get(&ws.focused_buffer())
        .map(|bf| bf.path.display().to_string());
    ws.set_special_registers(
        engine.last_search().map(str::to_string),
        last_ex_line.clone(),
        engine.last_inserted_text(),
        file,
    );
}

/// Dispatch the key after a `C-w` prefix (F-007 MVP window commands): `w`/`C-w` focus next, `s` split
/// horizontally, `v` split vertically, `c` close focused, `q` = `:quit`. An unrecognised key is ignored
/// (Vim beeps). The full `C-w` family is post-MVP.
fn dispatch_window(
    key: crossterm::event::KeyEvent,
    ws: &mut Workspace,
    quit: &mut bool,
    status: &mut String,
) {
    match key.code {
        KeyCode::Char('w') => ws.focus_next(),
        KeyCode::Char('s') => {
            ws.split(SplitDir::Horizontal);
        }
        KeyCode::Char('v') => {
            ws.split(SplitDir::Vertical);
        }
        KeyCode::Char('c') => {
            if !ws.close_focused() {
                *status = "E444: Cannot close last window".into();
            }
        }
        // `C-w q` is `:quit`: close the focused window (its buffer stays loaded, hidden, as with nvim's
        // default 'hidden'), or on the last window quit through the same unsaved-changes check (E37/E162).
        KeyCode::Char('q') => crate::app::dispatch::quit_window(ws, status, quit, false),
        _ => {}
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
        ws.place_focused_cursor(1); // on 'i'
        assert_eq!(
            info_status(&Command::AsciiInfo, &ws).as_deref(),
            Some("<i>  105,  Hex 69,  Octal 151")
        );
    }

    #[test]
    fn file_info_uses_name_and_line_count() {
        let mut ws = Workspace::new(&b"one\ntwo\nthree\n"[..]);
        ws.set_focused_buffer_name("f.txt");
        ws.place_focused_cursor(0); // line 1 of 3 -> 33%
        assert_eq!(
            info_status(&Command::FileInfo, &ws).as_deref(),
            Some("\"f.txt\" 3 lines --33%--")
        );
    }

    #[test]
    fn file_info_unnamed_buffer() {
        let mut ws = Workspace::new(&b"x\n"[..]);
        ws.place_focused_cursor(0);
        assert_eq!(
            info_status(&Command::FileInfo, &ws).as_deref(),
            Some("\"[No Name]\" 1 line --100%--")
        );
    }

    #[test]
    fn cursor_info_counts_over_the_buffer() {
        let mut ws = Workspace::new(&b"foo bar baz\n"[..]);
        ws.place_focused_cursor(4); // on 'b' of "bar" (byte 4, word 2)
        assert_eq!(
            info_status(&Command::CursorInfo, &ws).as_deref(),
            Some("Col 5 of 11; Line 1 of 1; Word 2 of 3; Byte 5 of 12")
        );
    }

    #[test]
    fn non_info_command_is_none() {
        let ws = Workspace::new(&b"x\n"[..]);
        assert!(info_status(&Command::Undo, &ws).is_none());
    }
}
