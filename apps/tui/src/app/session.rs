//! The event-loop hub: `run` drives the whole editor session — the `while !quit` loop that snapshots
//! the focused buffer, scrolls each pane, highlights the viewport, renders the frame, reads one key,
//! and routes it (confirm loop / overlays / window prefix / the input engine). Plus the two small
//! frontend intercepts it calls: `is_ctrl` and the `C-w` window-command dispatch.

use std::collections::{HashMap, HashSet};
use std::io;
use std::path::PathBuf;

use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::terminal as ct_terminal;

use ruse_core::{CaretGravity, Command, DocumentId, Mode, OpenKind, SplitDir, Workspace};

use crate::app::dispatch::{is_ctrl, open_file_into_buffer, run_cmd, run_ex, BufferFile, Files};
use crate::app::lsp_coordinator::LspCoordinator;
use crate::input::{parse_ex, Ex, Feed, InputEngine};
use crate::terminal::guard::TermGuard;
use crate::ui::palette::focused_context;
use crate::ui::picker::{PickOutcome, Picker};
use crate::ui::prompts::{confirm_key, confirm_prompt, prompt_recovery, Confirm};
use crate::ui::render::{render, search_pattern};
use crate::ui::{
    action_picker, buffer_picker, diag_picker, file_picker, layout::window_rects, line_picker,
    marks_picker, palette, pos_picker, ref_picker, register_picker,
};
use crate::{graphics, health, highlight, indent, line_index, persist, recover, screen, viewport};
#[cfg(unix)]
use crate::{pty, term_buffer};

/// Rows of context kept above and below the cursor when scrolling (Vim's `scrolloff`).
const SCROLLOFF: usize = 3;

/// Append a recovery-journal frame every Nth modified command — a coarse hard-kill safety net (a
/// panic captures the exact latest state separately). Full incremental journaling is post-MVP.
const JOURNAL_THROTTLE: u32 = 8;

/// F-011: when a terminal is live the event loop polls for a key with this timeout (≈30fps) instead of
/// blocking, so asynchronous PTY output renders within one tick. Only the terminal-active path polls.
#[cfg(unix)]
const TERM_TICK_MS: u64 = 33;

/// F-014: with only a language server live (no terminal), poll more slowly — diagnostics arrive infrequently,
/// so a ~10fps tick keeps them prompt without spinning.
const LSP_TICK_MS: u64 = 100;

pub(crate) fn run(path: Option<PathBuf>, raw: Vec<u8>) -> io::Result<()> {
    // F-008: detect the original encoding/line-ending once, edit in clean LF, restore it on save.
    let fmt = persist::encoding::FileFormat::detect(&raw);
    let disk = fmt.to_buffer(&raw);
    // A crash may have left an append-only journal of unsaved work; offer it, never auto-apply it.
    let recovered = persist::journal::replay(path.as_deref());
    let recovery = persist::assess_recovery(&disk, recovered.as_deref());

    // Syntax highlighting lives in the frontend (core stays dep-free); one highlighter PER file buffer is
    // built below in the `files` seed / on `:e`, keyed by DocumentId (F-007 multi-buffer).
    // hlsearch/incsearch match spans, cached on (revision, viewport, pattern) like the syntax highlighter
    // — no full-buffer regex per frame (F-009 #1).
    let mut cached_search = highlight::CachedSearch::default();
    // Revision-cached line index: O(log n) row/line lookups instead of an O(buffer) newline scan per
    // pane per frame (D-042 win D).
    let mut line_idx = line_index::LineIndex::default();

    let guard = TermGuard::enter()?;
    let mut out = io::stdout();

    // Open-time crash recovery (F-008 #3/#4): the user picks; the original file is never touched.
    let (initial, mut status) = match recovery {
        persist::Recovery::Available(rec) if prompt_recovery(&mut out)? => (
            rec,
            String::from("ruse — recovered unsaved changes (:w to save)"),
        ),
        persist::Recovery::Available(_) => {
            persist::journal::clear(path.as_deref()); // discarded on the user's say-so
            (
                disk,
                String::from("ruse — recovery discarded; opened disk version"),
            )
        }
        persist::Recovery::None => (disk, String::from("ruse — :q to quit")),
    };

    // F-007: the frontend now drives a Workspace (buffers + views + windows), not a single
    // EditorState. With one Window this is byte-identical to the pre-Workspace path; `:split`/
    // `:vsplit` open more Windows onto the same buffer with independent cursors and scroll.
    let mut ws = Workspace::new(initial.clone());
    // The initial buffer owns the session file (`path`/`fmt`): name it, and remember its id so `:w` on any
    // OTHER buffer (`:enew` scratch) declines rather than clobbering the file (F-007 multi-buffer).
    let file_buf = ws.focused_buffer();
    // The per-buffer file registry (F-007): the primary buffer joins it when opened with a path; `:e`
    // adds more, `:enew` scratch buffers stay out (so `:w` on them declines). `:w` writes files[focused].
    let mut files: Files = HashMap::new();
    let mut highlighters: HashMap<ruse_core::DocumentId, highlight::CachedHighlight> =
        HashMap::new();
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
    let mut recorded: Vec<Command> = Vec::new();
    let mut journal_ticks: u32 = 0; // throttle: append the recovery journal every Nth modified frame

    // Profile selection (F-012 / F-013): no config loader exists yet, so `RUSE_PROFILE` picks the profile
    // (the same env-override seam as the terminal caps). `emacs` = Emacs, `native` = Native, else Vim.
    let profile_name = match std::env::var("RUSE_PROFILE").as_deref() {
        Ok("emacs") => "emacs",
        Ok("native") => "native",
        _ => "vim",
    };
    let emacs_profile = profile_name == "emacs";
    let mut engine = match profile_name {
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
    let mut quit = false;
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
    let mut pending_window = false; // a `C-w` window-command prefix awaits its second key (F-007)
    let mut pending_z = false; // a `z` scroll prefix awaits its second key (`zz`/`zt`/`zb`)
                               // Vim macros (D-055): `q{a-z}` records the raw keystroke stream into a register, `@{a-z}` replays it.
                               // The whole record/replay state machine lives in `keys::MacroState` (unit-tested end to end).
    let mut macros = crate::keys::MacroState::new();
    let mut confirm: Option<Confirm> = None; // a `:s///c` interactive confirm loop, when active (F-009)
    let mut search_hl: Option<String> = None; // the hlsearch pattern (last `/`-search), until `:noh`
                                              // The last `:s` (pattern, replacement, flags) — recorded on every substitute so `&` can repeat it (F-009).
    let mut last_substitute: Option<(String, String, ruse_core::SubFlags)> = None;
    // The three modal picker overlays (F-004 / F-013 NAT-3), all `Picker<T>` over different payloads:
    // command palette (`C-p`), buffer-line jump (`C-l`), buffer switch (`C-b`). At most one is open.
    let mut palette: Option<Picker<Command>> = None;
    let mut line_picker: Option<Picker<usize>> = None;
    let mut buffer_picker: Option<Picker<DocumentId>> = None;
    let mut file_picker: Option<Picker<PathBuf>> = None; // fuzzy file finder (`C-f`), opens like `:e`
                                                         // F-014: the LSP references picker — `(uri, line, character)` locations; Enter jumps (same/cross file).
    let mut ref_picker: Option<Picker<(String, u32, u32)>> = None;
    // F-014: the LSP code-action picker — Enter applies the selected action's WorkspaceEdit.
    let mut action_picker: Option<Picker<crate::lsp::protocol::CodeAction>> = None;
    // F-014: the diagnostics list picker — payload is the diagnostic's start byte; Enter jumps there.
    let mut diag_picker: Option<Picker<usize>> = None;
    // F-029: the `:registers` viewer — payload is the register name; view-only (Enter just closes).
    let mut reg_picker: Option<Picker<char>> = None;
    // F-003: the `:marks` viewer — payload is the mark's byte offset; Enter jumps the cursor there.
    let mut marks_picker: Option<Picker<usize>> = None;
    // F-003: the shared `:jumps` / `:changes` position viewer — payload is a byte offset; Enter jumps.
    let mut pos_picker: Option<Picker<usize>> = None;
    // F-011: live terminal buffers keyed by their placeholder DocumentId (unix-only). `pending_term_escape`
    // tracks a `CTRL-\` awaiting `CTRL-N` (the Terminal → Terminal-Normal escape).
    #[cfg(unix)]
    let mut terminals: HashMap<DocumentId, term_buffer::Terminal> = HashMap::new();
    #[cfg(unix)]
    let mut pending_term_escape = false;
    // F-014: all app-side LSP orchestration (clients, diagnostics, hover/completion, request dispatch,
    // deferred edit-applies) lives behind the coordinator; the loop just drives it (CAP-LSP-COORD).
    let mut lsp = LspCoordinator::new(std::env::current_dir().unwrap_or_default());

    while !quit {
        // The FOCUSED buffer is the file on disk (splits share it; MVP is single-file). Snapshot it
        // once for the panic-rescue mirror, the recovery journal, and the highlight parse.
        let (revision, modified, snapshot) = {
            let f = ws.focused();
            (
                f.doc.revision(),
                f.doc.is_modified(),
                f.doc.bytes().to_vec(),
            )
        };
        // Refresh the line index (rebuilds only on a revision change) so the per-frame row/viewport
        // lookups below are O(log n), not an O(buffer) newline scan. MVP splits share this one buffer.
        line_idx.refresh(revision, &snapshot);
        // Panic-rescue mirror + recovery journal are keyed to the FOCUSED buffer's own file path (F-007):
        // a hard kill while editing ANY file-backed buffer (incl. a `:e`-opened one) persists it — the
        // journal is recovered by reopening that file. A scratch buffer (no `files` entry) is skipped so
        // its bytes never land in some file's recovery. `save()` already clears the focused buffer's
        // journal by path, so this stays consistent. (Full incremental journal design is post-MVP.)
        if let Some(bf) = files.get(&ws.focused_buffer()) {
            // Keep the in-memory snapshot current so a core panic can rescue unsaved work (§6/§8).
            recover::update(Some(&bf.path), &snapshot, modified);
            // Throttle an append-only journal frame so a hard kill (not just a panic) loses at most a few
            // edits. Cleared on a durable save.
            if modified {
                journal_ticks += 1;
                if journal_ticks.is_multiple_of(JOURNAL_THROTTLE) {
                    let _ = persist::journal::append(Some(bf.path.as_path()), &snapshot);
                }
            }
        }
        // During a `:s///c` confirm, follow the current match so the viewport scrolls it into view.
        if let Some(c) = &confirm {
            if let Some(s) = c.subs.get(c.idx) {
                ws.place_focused_cursor(s.start);
                status = confirm_prompt(c);
            }
        }
        // Per-window viewport pass: scroll each pane so ITS cursor stays visible in ITS rectangle
        // (F-007 acceptance #1 — independent scroll). Geometry is shared with render below.
        let (cols, term_rows) = ct_terminal::size().unwrap_or((80, 24));
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
        // Highlight only the VISIBLE byte range of the focused buffer (F-015 #3): the union of the
        // viewports of every pane showing it. The tree is reparsed incrementally on edit (keyed on
        // revision); the viewport-bounded query keeps the per-keystroke cost O(viewport), not O(buffer).
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
        let visible = if vis_start <= vis_end {
            vis_start..vis_end
        } else {
            0..snapshot.len()
        };
        // Syntax highlighting is the FOCUSED buffer's own grammar (F-007): each file buffer has its own
        // highlighter in the registry; a scratch/no-file buffer has none, so it paints unhighlighted.
        let focused_id = ws.focused_buffer();
        // F-031 3b-2b: reserve taller image blocks on a graphics-capable terminal (no-op after the first).
        if let Some(h) = highlighters.get_mut(&focused_id) {
            h.set_image_rows(image_rows);
        }
        let (spans, virt_lines): (&[highlight::Span], &[highlight::VirtLine]) =
            match highlighters.get_mut(&focused_id) {
                Some(h) => h.spans_and_virt(revision, &snapshot, visible.clone()),
                None => (&[], &[]),
            };
        // The focused pane's extra reverse-video highlights: a `:s///c` confirm match, else the
        // incsearch pattern being typed in `/`…`?`, else the last search (hlsearch) — F-009 #1. The
        // search matches are viewport-cached (CachedSearch), not a per-frame full-buffer regex.
        let focus_hl: Vec<(usize, usize)> = if let Some(c) = &confirm {
            c.subs
                .get(c.idx)
                .map(|s| vec![(s.start, s.end)])
                .unwrap_or_default()
        } else {
            let active = match engine.cmdline() {
                Some(('/', buf, _)) | Some(('?', buf, _)) if !buf.is_empty() => {
                    Some(buf.to_string())
                }
                _ => search_hl.clone(),
            };
            active
                .map(|p| {
                    cached_search
                        .spans(revision, &snapshot, visible.clone(), &p)
                        .to_vec()
                })
                .unwrap_or_default()
        };
        // Overlay match rows painted above the status line — at most one picker is open at a time, so all
        // three share the same uniform `rows()` paint slot (F-004 / F-013 NAT-3).
        let overlay_rows: Vec<(String, bool)> = if let Some(p) = palette.as_ref() {
            p.rows()
        } else if let Some(p) = line_picker.as_ref() {
            p.rows()
        } else if let Some(p) = buffer_picker.as_ref() {
            p.rows()
        } else if let Some(p) = file_picker.as_ref() {
            p.rows()
        } else if let Some(p) = ref_picker.as_ref() {
            p.rows()
        } else if let Some(p) = action_picker.as_ref() {
            p.rows()
        } else if let Some(p) = diag_picker.as_ref() {
            p.rows()
        } else if let Some(p) = reg_picker.as_ref() {
            p.rows()
        } else if let Some(p) = marks_picker.as_ref() {
            p.rows()
        } else if let Some(p) = pos_picker.as_ref() {
            p.rows()
        } else {
            // F-014: an LSP hover result shares the overlay slot (no picker can be open here).
            lsp.hover_overlay().unwrap_or_default()
        };
        // The Native leader (which-key) hint owns the command line while armed (F-013 NAT-2), shown with a
        // Space prefix — below the overlay, above the ordinary `:`/`/` line (none can co-occur).
        let leader_hint = engine.leader_hint();
        let cmd_line: Option<(char, &str)> = if let Some(p) = palette.as_ref() {
            Some(('>', p.query.as_str())) // command palette prompt
        } else if let Some(p) = line_picker.as_ref() {
            Some(('#', p.query.as_str())) // line-picker prompt (# = line jump)
        } else if let Some(p) = buffer_picker.as_ref() {
            Some(('@', p.query.as_str())) // buffer-picker prompt
        } else if let Some(p) = file_picker.as_ref() {
            Some(('~', p.query.as_str())) // file-picker prompt (~ = find file)
        } else if let Some(p) = ref_picker.as_ref() {
            Some(('*', p.query.as_str())) // references-picker prompt (* = references to symbol)
        } else if let Some(p) = action_picker.as_ref() {
            Some(('!', p.query.as_str())) // code-action-picker prompt (! = actions/fixes at cursor)
        } else if let Some(p) = diag_picker.as_ref() {
            Some(('✗', p.query.as_str())) // diagnostics-picker prompt
        } else if let Some(p) = reg_picker.as_ref() {
            Some(('"', p.query.as_str())) // registers-viewer prompt (" = registers)
        } else if let Some(p) = marks_picker.as_ref() {
            Some(('\'', p.query.as_str())) // marks-viewer prompt (' = marks)
        } else if let Some(p) = pos_picker.as_ref() {
            Some(('↕', p.query.as_str())) // jumps/changes position-viewer prompt
        } else if let Some(h) = leader_hint.as_ref() {
            Some((' ', h.as_str()))
        } else {
            engine.cmdline().map(|(pfx, t, _)| (pfx, t))
        };
        // F-011: reap terminals whose buffer was closed (Drop hangs up the child), resize each to its window
        // rect (so the child reflows), then pull pending PTY output into its VT grid so it paints this frame.
        // `term_views` lends each grid to the renderer. On non-unix this is always empty.
        #[cfg(unix)]
        let term_views: crate::ui::render::TermViews = {
            let live: std::collections::HashSet<DocumentId> =
                ws.buffers().iter().map(|b| b.id).collect();
            terminals.retain(|id, _| live.contains(id));
            for (i, rect) in rects.iter().enumerate().take(ws.window_count()) {
                let doc = ws.pane(i).view.doc();
                if let Some(t) = terminals.get_mut(&doc) {
                    t.resize(rect.h.max(1), rect.w.max(1));
                }
            }
            for t in terminals.values_mut() {
                t.drain();
            }
            terminals.iter().map(|(id, t)| (*id, t.grid())).collect()
        };
        #[cfg(not(unix))]
        let term_views = crate::ui::render::TermViews::new();
        // F-014: sync the focused buffer with its server + poll all clients (diagnostics + response
        // dispatch) — the coordinator owns all of it; deferred edits/refs are applied after render.
        lsp.sync_and_poll(&ws, &files, revision, &snapshot, &mut status);
        let focus_diags = lsp.diagnostics_for(ws.focused_buffer());
        // F-031 3b-2c: read each visible image's pixel dimensions (IHDR only) so render can size it to its
        // natural aspect and centre it. Cheap — only the 24-byte header, only when graphics are on.
        let image_dims: HashMap<String, (u32, u32)> = if has_graphics {
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
        } else {
            HashMap::new()
        };
        let images = render(
            &mut out,
            &ws,
            cmd_line,
            &status,
            spans,
            virt_lines,
            &rects,
            &mut prev_frame,
            sync_output,
            &focus_hl,
            &overlay_rows,
            &term_views,
            focus_diags,
            lsp.completion_view(),
            has_graphics,
            &image_dims,
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
        // F-014: apply the coordinator's deferred results now render is done (opening a buffer mutates
        // `highlighters`, which the frame's `spans` borrow forbade during the poll). Then open the
        // references picker (past `cmd_line`'s borrow of `ref_picker`).
        lsp.apply_pending(
            &mut ws,
            &mut files,
            &mut highlighters,
            &snapshot,
            &mut status,
        );
        if let Some(locs) = lsp.take_refs() {
            ref_picker = Some(ref_picker::open(locs, lsp.cwd()));
        }
        if let Some(actions) = lsp.take_actions() {
            action_picker = Some(action_picker::open(actions));
        }
        // Async output (PTY, F-011; LSP diagnostics, F-014) must render without a keypress, so the loop polls
        // with a timeout while either is live. With neither it keeps the pure blocking read (no spin,
        // unchanged) — a terminal ticks fast, an LSP-only session more slowly.
        let mut poll_ms: Option<u64> = None;
        #[cfg(unix)]
        if !terminals.is_empty() {
            poll_ms = Some(TERM_TICK_MS);
        }
        if poll_ms.is_none() && lsp.has_live_client() {
            poll_ms = Some(LSP_TICK_MS);
        }
        // Macro replay (D-055): drain the replay queue before reading the terminal, so `@{reg}`'s decoded
        // keys flow through the same dispatch as if re-typed. A key from the queue is `from_replay`.
        let (raw_key, from_replay) = match macros.next_replay() {
            Some(k) => (k, true),
            None => {
                let k = if let Some(ms) = poll_ms {
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
        if raw_key.kind == KeyEventKind::Release {
            continue;
        }
        // Run the key through the macro state machine (capture / stop / `q`|`@` prefixes). Only a
        // `Dispatch` falls through to the normal engine + intercepts; the rest are handled here.
        let macro_normal =
            matches!(ws.focused().view.mode(), Mode::Normal) && engine.cmdline().is_none();
        let key = match macros.step(raw_key, from_replay, macro_normal) {
            crate::keys::Step::Dispatch(k) => k,
            crate::keys::Step::Consumed => continue,
            crate::keys::Step::Store(reg, bytes) => {
                let n = bytes.len();
                ws.set_register_raw(Some(reg), bytes);
                status = format!("recorded @{reg} ({n} keys)");
                continue;
            }
            crate::keys::Step::Replay(reg) => {
                // `{count}@{reg}` repeats the macro `count` times; the count was accumulated by the engine
                // from the digits typed before `@`, so consume it here. The recursion budget is shared
                // across all copies, so `999@a` of a big macro still terminates.
                let n = engine.take_count().max(1);
                let bytes = ws.register_bytes(Some(reg));
                for _ in 0..n {
                    if !macros.replay(&bytes) {
                        status = "macro replay aborted (key limit)".to_string();
                        break;
                    }
                }
                continue;
            }
        };
        // F-014: any key dismisses a shown hover panel (a fresh `K` re-populates it after its response).
        lsp.clear_hover();
        // F-014 #5: the completion pum owns the keystream while open (nav / accept / dismiss); `<C-x><C-o>`
        // in Vim/Native Insert opens it. Each returns whether it consumed the key (so the loop `continue`s).
        if lsp.on_completion_key(key, &mut ws, &mut status) {
            continue;
        }
        if lsp.on_omni_key(
            key,
            &ws,
            &files,
            &snapshot,
            revision,
            emacs_profile,
            &mut status,
        ) {
            continue;
        }
        // A `:s///c` confirm loop owns the keystream while active: y/n/a/l/q per match (F-009 #2).
        if confirm.is_some() {
            confirm_key(&mut confirm, key, &mut ws, &mut status);
            continue;
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
                line_picker = None;
            }
            continue;
        }
        // The buffer picker switches the focused window to the selected buffer.
        if let Some(outcome) = buffer_picker.as_mut().map(|p| p.on_key(key)) {
            if let PickOutcome::Accept = outcome {
                if let Some(id) = buffer_picker.as_ref().and_then(|p| p.selected().copied()) {
                    ws.focus_buffer(id);
                }
            }
            if !matches!(outcome, PickOutcome::Continue) {
                buffer_picker = None;
            }
            continue;
        }
        // The file picker opens the selected path into a new buffer (the interactive form of `:e`).
        if let Some(outcome) = file_picker.as_mut().map(|p| p.on_key(key)) {
            if let PickOutcome::Accept = outcome {
                if let Some(p) = file_picker.as_ref().and_then(|p| p.selected().cloned()) {
                    status = open_file_into_buffer(
                        &p.display().to_string(),
                        &mut ws,
                        &mut files,
                        &mut highlighters,
                    );
                }
            }
            if !matches!(outcome, PickOutcome::Continue) {
                file_picker = None;
            }
            continue;
        }
        // F-014: the references picker jumps to the selected location. The `spans` borrow is released by
        // now (render is done), so opening a cross-file buffer here is free — no defer needed (unlike the
        // response-loop goto). Same file → move; other file → open then move (the goto path, inline).
        if let Some(outcome) = ref_picker.as_mut().map(|p| p.on_key(key)) {
            if let PickOutcome::Accept = outcome {
                if let Some((uri, l, c)) = ref_picker.as_ref().and_then(|p| p.selected().cloned()) {
                    let path = uri.strip_prefix("file://").unwrap_or(&uri).to_string();
                    let target =
                        std::fs::canonicalize(&path).unwrap_or_else(|_| PathBuf::from(&path));
                    let cur_path = files
                        .get(&ws.focused_buffer())
                        .and_then(|bf| std::fs::canonicalize(&bf.path).ok());
                    if cur_path.as_deref() == Some(target.as_path()) {
                        let bytes = ws.focused().doc.bytes().to_vec();
                        ws.place_focused_cursor(crate::lsp::model::lsp_pos_to_byte(&bytes, l, c));
                    } else {
                        open_file_into_buffer(
                            &target.display().to_string(),
                            &mut ws,
                            &mut files,
                            &mut highlighters,
                        );
                        let bytes = ws.focused().doc.bytes().to_vec();
                        ws.place_focused_cursor(crate::lsp::model::lsp_pos_to_byte(&bytes, l, c));
                    }
                }
            }
            if !matches!(outcome, PickOutcome::Continue) {
                ref_picker = None;
            }
            continue;
        }
        // F-014: the code-action picker applies the selected action's WorkspaceEdit (the multi-file apply,
        // inline here since the `spans` borrow is released post-render — like the references jump).
        if let Some(outcome) = action_picker.as_mut().map(|p| p.on_key(key)) {
            if let PickOutcome::Accept = outcome {
                if let Some(action) = action_picker.as_ref().and_then(|p| p.selected().cloned()) {
                    lsp.apply_code_action(
                        &action,
                        &mut ws,
                        &mut files,
                        &mut highlighters,
                        &mut status,
                    );
                }
            }
            if !matches!(outcome, PickOutcome::Continue) {
                action_picker = None;
            }
            continue;
        }
        // F-014: the diagnostics picker jumps the cursor to the selected diagnostic's byte offset.
        if let Some(outcome) = diag_picker.as_mut().map(|p| p.on_key(key)) {
            if let PickOutcome::Accept = outcome {
                if let Some(off) = diag_picker.as_ref().and_then(|p| p.selected().copied()) {
                    ws.place_focused_cursor(off);
                }
            }
            if !matches!(outcome, PickOutcome::Continue) {
                diag_picker = None;
            }
            continue;
        }
        // F-029: the `:registers` viewer is view-only — any non-Continue outcome (Enter / Esc) just closes it.
        if let Some(outcome) = reg_picker.as_mut().map(|p| p.on_key(key)) {
            if !matches!(outcome, PickOutcome::Continue) {
                reg_picker = None;
            }
            continue;
        }
        // F-003: the `:marks` viewer jumps the cursor to the selected mark's byte offset on Enter.
        if let Some(outcome) = marks_picker.as_mut().map(|p| p.on_key(key)) {
            if let PickOutcome::Accept = outcome {
                if let Some(off) = marks_picker.as_ref().and_then(|p| p.selected().copied()) {
                    ws.place_focused_cursor(off);
                }
            }
            if !matches!(outcome, PickOutcome::Continue) {
                marks_picker = None;
            }
            continue;
        }
        // F-003: the `:jumps` / `:changes` position viewer jumps the cursor to the selected offset on Enter.
        if let Some(outcome) = pos_picker.as_mut().map(|p| p.on_key(key)) {
            if let PickOutcome::Accept = outcome {
                if let Some(off) = pos_picker.as_ref().and_then(|p| p.selected().copied()) {
                    ws.place_focused_cursor(off);
                }
            }
            if !matches!(outcome, PickOutcome::Continue) {
                pos_picker = None;
            }
            continue;
        }
        // The command palette dispatches the selected command by its stable id, through the normal command
        // path (so it undoes/records like any other).
        if let Some(outcome) = palette.as_mut().map(|p| p.on_key(key)) {
            if let PickOutcome::Accept = outcome {
                let cmd = palette.as_ref().and_then(|p| p.selected().cloned());
                if let Some(cmd) = cmd {
                    run_cmd(cmd, &mut ws, &files, &mut recorded, &mut status, &mut quit);
                }
            }
            if !matches!(outcome, PickOutcome::Continue) {
                palette = None;
            }
            continue;
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
                        if pending_term_escape {
                            pending_term_escape = false;
                            if is_ctrl(key, 'n') {
                                run_cmd(
                                    Command::EnterTerminalNormal,
                                    &mut ws,
                                    &files,
                                    &mut recorded,
                                    &mut status,
                                    &mut quit,
                                );
                                continue;
                            }
                            // Not the escape: the swallowed `CTRL-\` is dropped; fall through to send `key`.
                        }
                        if is_ctrl(key, '\\') {
                            pending_term_escape = true;
                            continue;
                        }
                        if let Some(bytes) = pty::encode_key(key) {
                            if let Some(t) = terminals.get_mut(&tid) {
                                t.send(&bytes);
                            }
                        }
                        continue;
                    }
                    Mode::TerminalNormal => {
                        if matches!(key.code, KeyCode::Char('i' | 'a' | 'A')) {
                            run_cmd(
                                Command::EnterTerminal,
                                &mut ws,
                                &files,
                                &mut recorded,
                                &mut status,
                                &mut quit,
                            );
                            continue;
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
        // `z` scroll prefix: the second key recenters the view on the cursor line (`zz`/`z.` center,
        // `zt`/`z<CR>` top, `zb`/`z-` bottom). Only `top` moves; the per-frame scroll pass then applies
        // `scrolloff`, so `zt`/`zb` land the line `scrolloff` rows inside the edge, as in Vim.
        if pending_z {
            pending_z = false;
            let to = match key.code {
                KeyCode::Char('z' | '.') => Some(viewport::RecenterTo::Center),
                KeyCode::Char('t') | KeyCode::Enter => Some(viewport::RecenterTo::Top),
                KeyCode::Char('b' | '-') => Some(viewport::RecenterTo::Bottom),
                _ => None,
            };
            if let Some(to) = to {
                let row = line_idx.line_of(ws.focused().view.cursor());
                ws.set_top(ws.focus(), viewport::recenter(row, focused_h, to));
            }
            continue;
        }
        // F-007 window layer: a `C-w` prefix (Normal mode, no command-line, not Insert where `C-w`
        // deletes a word) arms the next key as a window command. A thin frontend intercept for MVP;
        // F-003's keymap router will absorb it into a proper layer.
        if pending_window {
            pending_window = false;
            dispatch_window(key, &mut ws, &mut quit);
            continue;
        }
        let normal = matches!(ws.focused().view.mode(), Mode::Normal) && engine.cmdline().is_none();
        if normal && is_ctrl(key, 'w') {
            pending_window = true;
            continue;
        }
        // F-014: `K` = hover, `<C-]>` = goto-definition (when the focused buffer has a live server; else
        // both fall through, unbound in the grammar). The coordinator sends the request.
        if lsp.on_normal_key(key, normal, &ws, &files, &snapshot) {
            continue;
        }
        // `C-p` opens the command palette (F-004 #2), context-filtered to the focused view.
        if normal && is_ctrl(key, 'p') {
            palette = Some(palette::open(&focused_context(&ws), &engine));
            continue;
        }
        // `C-l` opens the buffer-line fuzzy picker (F-013 NAT-3). Normal-only, so the Emacs profile's
        // non-modal C-l (recenter) is unaffected; C-l is unbound in the Vim/Native Normal grammar.
        if normal && is_ctrl(key, 'l') {
            line_picker = Some(line_picker::open(ws.focused().doc.bytes()));
            continue;
        }
        // `C-b` opens the buffer picker (F-013 NAT-3). Normal-only, so the Emacs profile's non-modal C-b
        // (backward-char) is unaffected; C-b is unbound in the Vim/Native Normal grammar (no page-scroll).
        if normal && is_ctrl(key, 'b') {
            buffer_picker = Some(buffer_picker::open(&ws));
            continue;
        }
        // `C-f` opens the fuzzy file finder (F-013 NAT-3). Normal-only, so the Emacs non-modal C-f
        // (forward-char) is unaffected; in the Vim/Native Normal grammar the plain `f` find-char has no
        // ctrl, so intercepting ctrl-f shadows nothing reachable.
        if normal && is_ctrl(key, 'f') {
            file_picker = Some(file_picker::open());
            continue;
        }
        // `z` arms the scroll-prefix (handled above on the next key). Normal-only, plain `z` (no ctrl).
        if normal && key.code == KeyCode::Char('z') && key.modifiers.is_empty() {
            pending_z = true;
            continue;
        }
        // `&` repeats the last `:s` on the CURRENT line WITHOUT its flags (Vim `&`). No-op before any `:s`.
        if normal && key.code == KeyCode::Char('&') && key.modifiers.is_empty() {
            status = match &last_substitute {
                Some((pat, rep, _flags)) => match ws.substitute(
                    ruse_core::SubRange::CurrentLine,
                    pat,
                    rep,
                    ruse_core::SubFlags::default(), // `&` drops the previous flags (Vim)
                ) {
                    Ok(out) if out.replacements == 0 => format!("E486: pattern not found: {pat}"),
                    Ok(out) => format!("{} substitutions on {} lines", out.replacements, out.lines),
                    Err(e) => crate::app::dispatch::regex_error_msg(&e),
                },
                None => "no previous substitute".to_string(),
            };
            continue;
        }
        // `C-d` / `C-u` scroll a half page: move the cursor half the pane down / up (column preserved via
        // the core Move), and the per-frame scroll pass follows it. `C-f`/`C-b` are taken by the pickers.
        if normal && (is_ctrl(key, 'd') || is_ctrl(key, 'u')) {
            engine.take_count(); // consume any pending count so it can't leak onto the next command
            let half = (focused_h / 2).max(1) as u32;
            let m = if is_ctrl(key, 'd') {
                ruse_core::Motion::Down
            } else {
                ruse_core::Motion::Up
            };
            run_cmd(
                Command::Move(half, m),
                &mut ws,
                &files,
                &mut recorded,
                &mut status,
                &mut quit,
            );
            continue;
        }
        // `C-e` / `C-y` scroll the view one line down / up, nudging the cursor only if it would leave the
        // scrolloff band. Frontend-only (viewport concern); `{count}` scrolls that many lines.
        if normal && (is_ctrl(key, 'e') || is_ctrl(key, 'y')) {
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
                    &mut ws,
                    &files,
                    &mut recorded,
                    &mut status,
                    &mut quit,
                );
            }
            continue;
        }
        // `H` / `M` / `L` — move the cursor to the top / middle / bottom visible line (first non-blank via
        // GotoLine). Non-operator (frontend intercept); operator-compatible `dH` needs viewport in the core.
        if normal
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
            let count = engine.take_count();
            let last_line = line_idx.line_of(snapshot.len());
            let row = viewport::screen_line(
                ws.focused().view.top(),
                focused_h,
                SCROLLOFF,
                to,
                count,
                last_line,
            );
            run_cmd(
                Command::Move(row as u32 + 1, ruse_core::Motion::GotoLine),
                &mut ws,
                &files,
                &mut recorded,
                &mut status,
                &mut quit,
            );
            continue;
        }
        // Every other key — command-line included — goes through the engine (F-026): the command-line
        // namespace owns its buffer, so the frontend no longer special-cases `:`/`/` typing.
        match engine.feed(key, ws.focused().view.mode()) {
            // A finished `:`-line (F-026): parse + run it. `submit_search` already folded a `/`-line
            // into `Feed::Cmd` inside the engine, so the frontend only sees the ex case here.
            Feed::ExecuteEx(text) => {
                match parse_ex(&text) {
                    Ex::NoHighlight => search_hl = None, // `:noh` clears the search highlight (F-009 #1)
                    // `:fmt` / `:rename {new}` / `:references` / `:codeaction` (F-014): the coordinator sends
                    // the request; the response is dispatched + applied (or opens a picker) on a later frame.
                    ex @ (Ex::Format | Ex::Rename(_) | Ex::References | Ex::CodeAction) => {
                        lsp.on_ex(&ex, &ws, &files, &snapshot, &mut status);
                    }
                    // `:diagnostics` (F-014): open a picker over the focused buffer's already-collected
                    // diagnostics (no server round-trip); Enter jumps to the selected one.
                    Ex::Diagnostics => {
                        let diags = lsp.diagnostics_for(ws.focused_buffer());
                        if diags.is_empty() {
                            status = "no diagnostics".to_string();
                        } else {
                            status = format!("{} diagnostic(s)", diags.len());
                            diag_picker = Some(diag_picker::open(diags, &snapshot));
                        }
                    }
                    // `:registers` (F-029): open a view-only picker over the non-empty registers, so a
                    // recorded macro (`"a`) or a yank is inspectable.
                    Ex::Registers => {
                        let snapshot = ws.register_snapshot();
                        if snapshot.is_empty() {
                            status = "no registers set".to_string();
                        } else {
                            status = format!("{} register(s)", snapshot.len());
                            reg_picker = Some(register_picker::open(snapshot));
                        }
                    }
                    // `:marks` (F-003): open a picker over the set marks; Enter jumps to the selected one.
                    Ex::Marks => {
                        let snapshot = ws.marks_snapshot();
                        if snapshot.is_empty() {
                            status = "no marks set".to_string();
                        } else {
                            status = format!("{} mark(s)", snapshot.len());
                            marks_picker =
                                Some(marks_picker::open(snapshot, ws.focused().doc.bytes()));
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
                            status = "empty".to_string();
                        } else {
                            status = format!("{} position(s)", positions.len());
                            pos_picker =
                                Some(pos_picker::open(positions, ws.focused().doc.bytes()));
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
                                    let id =
                                        ws.add_buffer(Vec::new(), Some("[terminal]".to_string()));
                                    ws.focus_buffer(id);
                                    terminals.insert(id, term);
                                    run_cmd(
                                        Command::EnterTerminal,
                                        &mut ws,
                                        &files,
                                        &mut recorded,
                                        &mut status,
                                        &mut quit,
                                    );
                                    status =
                                        "terminal started (CTRL-\\ CTRL-N to leave)".to_string();
                                }
                                Err(e) => status = format!("E: cannot start terminal: {e}"),
                            }
                        }
                        #[cfg(not(unix))]
                        {
                            status = "terminal is not supported on this platform".to_string();
                        }
                    }
                    // Lang-Arg map maintenance (F-027): the map is engine state, so it is applied here
                    // where `engine` is in scope, not in `run_ex` (which owns only workspace/file state).
                    Ex::Lmap { lhs, rhs } => engine.set_lang_mapping(lhs, rhs),
                    Ex::Lunmap { lhs } => engine.clear_lang_mapping(lhs),
                    // `:checkhealth` (F-030): gather the frontend snapshot HERE (guard/profile/highlighter
                    // are in scope, not in `run_ex`) and render the report's one-line summary into status.
                    Ex::CheckHealth => {
                        let inputs = health::HealthInputs {
                            profile: profile_name,
                            caret: if emacs_profile {
                                "between-char"
                            } else {
                                "on-char"
                            },
                            sgr_mouse: guard.sgr_mouse(),
                            sync_output: guard.sync_output(),
                            bracketed_paste: guard.bracketed_paste(),
                            file_ext: files
                                .get(&ws.focused_buffer())
                                .and_then(|bf| bf.path.extension())
                                .map(|e| e.to_string_lossy().into_owned()),
                            grammar_ok: highlighters.contains_key(&ws.focused_buffer()),
                            buffers: ws.window_count(),
                            trace_commands: recorded.len(),
                        };
                        status = health::summary_line(&health::report(&inputs));
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
                            run_cmd(
                                cmd.clone(),
                                &mut ws,
                                &files,
                                &mut recorded,
                                &mut status,
                                &mut quit,
                            );
                        }
                        status =
                            format!("{n} change(s) {}", if older { "earlier" } else { "later" });
                    }
                    // `:e {file}` opens a file into a new buffer (F-007) — shared with the file picker.
                    Ex::Edit(file) => {
                        status =
                            open_file_into_buffer(&file, &mut ws, &mut files, &mut highlighters);
                    }
                    // `:e!` reloads the focused buffer's file from disk, discarding unsaved changes.
                    Ex::EditReload => {
                        let id = ws.focused_buffer();
                        match files.get(&id).map(|bf| bf.path.clone()) {
                            Some(path) => match std::fs::read(&path) {
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
                                    status = format!("\"{}\" reloaded", path.display());
                                }
                                Err(e) => {
                                    status = format!("E484: can't open {}: {e}", path.display());
                                }
                            },
                            None => status = "E32: No file name".into(),
                        }
                    }
                    // `:bd` deletes the focused buffer — done HERE so the buffer's `files`/highlighter
                    // entries are dropped (both are `&mut` in scope, not in `run_ex`). Guards unsaved
                    // changes with E89 unless `!` forces it.
                    Ex::BufferDelete { force } => {
                        let id = ws.focused_buffer();
                        if !force && ws.focused().doc.is_modified() {
                            status = "E89: No write since last change (add ! to override)".into();
                        } else {
                            ws.remove_buffer(id);
                            files.remove(&id);
                            highlighters.remove(&id);
                            status = format!("buffer {} deleted", id.0);
                        }
                    }
                    ex => {
                        // Record a `:s` so `&` can repeat it (Vim). A non-empty pattern only — an empty
                        // pattern reuses the last search, which is not modelled here.
                        if let Ex::Substitute(spec) = &ex {
                            if !spec.pattern.is_empty() {
                                last_substitute = Some((
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
                            &ex,
                            &mut ws,
                            &files,
                            &initial,
                            &recorded,
                            &mut status,
                            &mut quit,
                            &mut confirm,
                        )
                    }
                }
            }
            Feed::Pending | Feed::Ignored => {}
            Feed::Cmd(cmd) => {
                // `g&` — repeat the last `:s` over the WHOLE FILE with its flags. Resolved here (the engine
                // has no substitute history); like the current-line `&`, it calls `substitute` directly.
                if matches!(cmd, Command::RepeatSubstituteGlobal) {
                    status = match &last_substitute {
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
                    continue;
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
                            engine.set_last_search(pat.clone());
                            if forward {
                                Command::SearchNext(pat)
                            } else {
                                Command::SearchPrev(pat)
                            }
                        }
                        None => {
                            status = "E348: No string under cursor".into();
                            continue;
                        }
                    }
                } else {
                    cmd
                };
                // Tree-aware `=` (F-015): if the focused buffer has a live syntax tree, resolve the reindent
                // range and compute the levels from the tree, then rewrite to a concrete `SetIndents` (the
                // trace replays it exactly). Without a tree it falls through to the core bracket-depth `=`.
                let cmd = if let Command::Reindent { count, motion } = cmd {
                    let id = ws.focused_buffer();
                    match (
                        ws.reindent_range(motion, count),
                        highlighters
                            .get(&id)
                            .and_then(highlight::CachedHighlight::tree),
                    ) {
                        (Some((first_line, last_line)), Some(tree)) => Command::SetIndents {
                            first_line,
                            last_line,
                            levels: indent::indent_levels(tree, &snapshot, first_line, last_line),
                        },
                        _ => Command::Reindent { count, motion },
                    }
                } else {
                    cmd
                };
                // Auto-indent on newline (F-015 Phase 2): when the focused buffer has a live syntax tree,
                // seed the line opened by `o`/`O`/`<CR>` with the tree-suggested indent, rewriting to
                // `OpenLineIndent`. The core recomputes the insertion point from the cursor — here we only
                // supply the level (the tree query offset mirrors where the newline lands). No tree ⇒ the
                // plain open (column-0), unchanged. Dot-repeat (`.`) replays the plain open via `Feed::Replay`
                // (below), so it degrades to column-0 — mirroring Phase 1's `=`, and avoiding replaying a
                // stale recorded level at a new location.
                let cmd = if matches!(
                    cmd,
                    Command::OpenBelow | Command::OpenAbove | Command::InsertNewline
                ) {
                    let id = ws.focused_buffer();
                    match highlighters
                        .get(&id)
                        .and_then(highlight::CachedHighlight::tree)
                    {
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
                                Command::OpenAbove => {
                                    (OpenKind::Above, line_idx.nth_line_start(row), row)
                                }
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
                // Closer auto-dedent (F-015 Phase 3a): a `}`/`)`/`]` typed in a tree-backed (code) buffer
                // realigns the line to its matching opener. Gated on a live tree so plain-text editing is
                // untouched; the core's bytes bracket-match decides whether to realign. Dot-repeat replays
                // the plain `InsertChar` (column unchanged), like Phase 2.
                let cmd = match cmd {
                    Command::InsertChar(c @ ('}' | ')' | ']'))
                        if highlighters
                            .get(&ws.focused_buffer())
                            .and_then(highlight::CachedHighlight::tree)
                            .is_some() =>
                    {
                        Command::InsertCloser { ch: c }
                    }
                    other => other,
                };
                // A completed search turns on hlsearch for that pattern (F-009 #1).
                if let Some(p) = search_pattern(&cmd) {
                    search_hl = Some(p);
                }
                run_cmd(cmd, &mut ws, &files, &mut recorded, &mut status, &mut quit);
            }
            // `.` (dot-repeat) replays the last change; record and apply each concrete command so the
            // trace (F-022) captures the resolved edit, not the `.` keypress.
            Feed::Replay(cmds) => {
                for cmd in cmds {
                    run_cmd(cmd, &mut ws, &files, &mut recorded, &mut status, &mut quit);
                }
            }
        }
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

/// Dispatch the key after a `C-w` prefix (F-007 MVP window commands): `w`/`C-w` focus next, `s` split
/// horizontally, `v` split vertically, `c` close focused, `q` close (or quit on the last window). An
/// unrecognised key is ignored (Vim beeps). The full `C-w` family is post-MVP.
fn dispatch_window(key: crossterm::event::KeyEvent, ws: &mut Workspace, quit: &mut bool) {
    match key.code {
        KeyCode::Char('w') => ws.focus_next(),
        KeyCode::Char('s') => {
            ws.split(SplitDir::Horizontal);
        }
        KeyCode::Char('v') => {
            ws.split(SplitDir::Vertical);
        }
        KeyCode::Char('c') => {
            ws.close_focused();
        }
        // `C-w q`: close the focused window, or quit if it was the last one.
        KeyCode::Char('q') => *quit = !ws.close_focused(),
        _ => {}
    }
}
