//! The event loop `run` drives (#1355): the start-up (journal offer, recovery prompt, config, the
//! session), one frame per pass (pre-frame, paint, post-render) and the key read.

use super::normal_keys::focused_folds;
use super::*;

/// `read_only_mode` is `ruse -R` (#1211).
pub(crate) fn run(
    path: Option<PathBuf>,
    raw: Vec<u8>,
    read_only_mode: bool,
    safe_mode: bool,
    binary: bool,
) -> io::Result<()> {
    let (fmt, disk, offer) = open_offer(path.as_deref(), &raw, binary);
    let guard = TermGuard::enter()?;
    let mut out = io::stdout();
    // Create crossterm's event source now, so its SIGWINCH registration exists before a fatal signal can
    // arrive: the signal handler wakes a blocked read through it (#857). Installed before the recovery
    // prompt, so a signal there also unwinds through the guard (#897).
    let _ = event::poll(std::time::Duration::ZERO);
    crate::signals::install();

    let Some(Opening {
        recovered,
        read_only,
        status,
        fmt,
    }) = ask_recovery(&mut out, offer, path.as_deref(), &disk, fmt)?
    else {
        // The journal stays for the next start.
        return Ok(());
    };
    let initial = disk;

    let Startup {
        cwd,
        config,
        config_paths,
        boot_record,
        mut boot_armed,
        status,
        profile,
        trusted,
    } = load_startup(safe_mode, status);
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
    if let Some(p) =
        s.ed.files
            .get(&s.ed.ws.focused_buffer())
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
        for bf in s.ed.files.values_mut() {
            bf.read_only = true; // the start buffer is the only one with a file yet
        }
    }
    s.readonly.mode = read_only_mode;
    s.ed.files.write.binary_mode = binary;
    // `"+`/`"*` (`:help quoteplus`): inject the real OS-clipboard provider (shell-out to pbcopy/wl-copy/
    // xclip/clip, resolved once). Degrades to a no-op when no clipboard tool is on PATH, so a headless run
    // still works — `"+p` just pastes nothing.
    s.ed.ws
        .set_clipboard(Box::new(crate::clipboard::SystemClipboard::detect()));
    // CTRL-C (`C-g` in Emacs) interrupts a long command: the core's checkpoints poll the terminal (#898).
    crate::breakcheck::install(profile == "emacs");

    let mut painter = Painter::new(&guard);
    // A start that crashes after loading the config and before its first frame, on demand: the crash
    // loop safe mode detects, for the end-to-end test (`tests/pty_config_safe.rs`).
    if std::env::var_os("RUSE_TEST_CRASH_BEFORE_FIRST_FRAME").is_some() {
        std::process::abort();
    }

    while !s.ed.quit {
        // SIGTERM / SIGHUP (#857): end the session; the caller journals every modified buffer and the
        // guard restores the terminal. The handler's wake-up lands here via a resize event.
        if crate::signals::pending().is_some() {
            break;
        }
        let (cols, term_rows) = ct_terminal::size().unwrap_or((80, 24));
        s.idle_disk_check();
        s.idle_auto_save_visited();
        let frame = s.pre_frame(cols, term_rows);
        if painter.due(s.replaying()) {
            painter.paint(&mut out, &mut s, &frame)?;
        }
        s.post_render(&frame);
        // The first frame is drawn: the config booted, so this start is no crash (`config::boot`).
        if let Some(b) = boot_armed.take() {
            b.clear();
        }
        #[cfg(debug_assertions)]
        crate::log::debug_panic_if_requested();
        let Some((raw_key, from_replay)) = next_key(&mut s, &frame)? else {
            continue;
        };
        if raw_key.kind != KeyEventKind::Release && s.answer_prompt(raw_key) {
            continue;
        }
        s.on_key(raw_key, from_replay, &frame);
    }
    // A normal quit lets every buffer's unsaved changes go (#862): `:q` only gets here with none, and
    // `:q!` / `:qa!` / `:cq` discard them on purpose. nvim deletes its swap files on each of these exits, so
    // the next start offers nothing. A signal ends the loop without `quit`, and its journals are kept.
    if s.ed.quit && crate::signals::pending().is_none() {
        s.discard_journals();
    }
    painter.clear_images(&mut out);
    Ok(())
}

/// What the start buffer opens with once the recovery prompt is answered.
struct Opening {
    /// The recovered text to apply over the disk text, if the user took it.
    recovered: Option<Vec<u8>>,
    read_only: bool,
    /// The start-up status line.
    status: String,
    /// The buffer's format: the journal's for a recovered buffer, else the disk file's.
    fmt: persist::encoding::FileFormat,
}

/// Open-time crash recovery (F-008 #3/#4, RFC-0023 §7): the user picks; the original file is never
/// touched. The buffer always starts as the disk text, which is also the trace's starting text; a
/// recovery is applied over it once the session exists, as an edit (#863). `None` when the user quits.
fn ask_recovery(
    out: &mut io::Stdout,
    offer: persist::Recovery,
    path: Option<&std::path::Path>,
    disk: &[u8],
    fmt: persist::encoding::FileFormat,
) -> io::Result<Option<Opening>> {
    let shown = path.map_or_else(
        || "the unnamed buffer".to_string(),
        |p| p.display().to_string(),
    );
    let (recovered, read_only, status, recovered_format) =
        match crate::ui::recovery::resolve(out, offer, &shown, path, disk, fmt, "quit")? {
            crate::ui::recovery::Resolved::Open {
                recovered,
                read_only,
                note,
                format,
            } => (
                recovered,
                read_only,
                note.map_or_else(
                    || String::from("ruse — :q to quit"),
                    |n| format!("ruse — {n}"),
                ),
                format,
            ),
            // The journal stays for the next start.
            crate::ui::recovery::Resolved::Quit { .. } => return Ok(None),
        };
    // A recovered buffer takes the format its journal recorded over the disk file's (#1286).
    let fmt = recovered_format.unwrap_or(fmt);
    Ok(Some(Opening {
        recovered,
        read_only,
        status,
        fmt,
    }))
}

/// F-008: read the file's format, then the recovery journal's offer for it: the format, the buffer text,
/// and what the journal offers.
fn open_offer(
    path: Option<&std::path::Path>,
    raw: &[u8],
    binary: bool,
) -> (persist::encoding::FileFormat, Vec<u8>, persist::Recovery) {
    // Detect the original encoding/line-ending once, edit in clean LF, restore it on save. `-b` reads
    // the bytes as they are (#619).
    let fmt = if binary {
        persist::encoding::FileFormat::binary(raw)
    } else {
        persist::encoding::FileFormat::detect(raw)
    };
    let disk = fmt.to_buffer(raw);
    // A crash may have left a journal of unsaved work; offer it, never auto-apply it. A journal that exists
    // but cannot be read or replayed is reported on the status line, never silently skipped (#897). The
    // owner (and the unnamed buffer's journal name) is fixed now, at startup (#1192).
    let _ = persist::journal::owner();
    // RFC-0023 §9 (#934): at most once a day, old archives go; live journals are never touched.
    if let Some(j) = persist::journal::current() {
        let _ = persist::retention::sweep_if_due(j, persist::journal::now_ms());
    }
    let offer =
        persist::journal::current().map_or(persist::Recovery::None, |j| j.offer(path, &disk));
    (fmt, disk, offer)
}

/// What the start-up reads before the session exists: the config layers and what they decide.
struct Startup {
    cwd: PathBuf,
    config: crate::config::Config,
    config_paths: crate::config::ConfigPaths,
    boot_record: Option<crate::config::boot::BootRecord>,
    boot_armed: Option<crate::config::boot::BootRecord>,
    /// The start-up status line, with the config notice.
    status: String,
    profile: &'static str,
    trusted: bool,
}

/// Load the config layers and decide safe mode, the profile and workspace trust. `status` is the start-up
/// status line so far.
fn load_startup(safe_mode: bool, status: String) -> Startup {
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
    let boot_armed = crate::config::boot::arm(
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
    Startup {
        cwd,
        config,
        config_paths,
        boot_record,
        boot_armed,
        status,
        profile,
        trusted,
    }
}

/// The paint side of the loop: the last frame's cells, the search-match cache, the terminal's graphics
/// support and the images it holds.
struct Painter {
    /// hlsearch/incsearch match spans, cached on (revision, viewport, pattern) like the syntax highlighter
    /// — no full-buffer regex per frame (F-009 #1).
    cached_search: highlight::CachedSearch,
    /// The previous frame's cell grid — the render diff emits only what changes against it (F-006).
    prev_frame: screen::Screen,
    /// Pinned once from the F-010 ledger (INV-RENDER-PROFILE).
    sync_output: bool,
    /// F-031: the terminal speaks the Kitty graphics protocol; only Kitty is lowered, so a Sixel/iTerm2
    /// terminal falls to the placeholder.
    has_graphics: bool,
    /// A graphics-capable terminal reserves taller image blocks (room for pixels).
    image_rows: u16,
    /// Inside tmux the graphics escapes are wrapped so tmux forwards them (needs `allow-passthrough on`).
    in_tmux: bool,
    resident: HashSet<graphics::ImageId>,
    last_paint: std::time::Instant,
}

impl Painter {
    fn new(guard: &TermGuard) -> Painter {
        let has_graphics = guard.graphics() == crate::caps::ledger::GraphicsProtocol::Kitty;
        Painter {
            cached_search: highlight::CachedSearch::default(),
            // Starts empty so the first frame paints in full.
            prev_frame: screen::Screen::new(0, 0),
            sync_output: guard.sync_output(),
            has_graphics,
            image_rows: if has_graphics { 12 } else { 2 },
            in_tmux: std::env::var_os("TMUX").is_some(),
            resident: HashSet::new(),
            last_paint: std::time::Instant::now(),
        }
    }

    /// Whether this pass paints. A macro replay repaints at most every `REPLAY_PAINT`, not once per
    /// replayed key: nvim does not redraw while it executes typeahead, and a frame per key made a
    /// 100,000-key replay take half a minute (#787). A typed key is always painted.
    fn due(&mut self, replaying: bool) -> bool {
        let paint = !replaying || self.last_paint.elapsed() >= REPLAY_PAINT;
        if paint {
            self.last_paint = std::time::Instant::now();
        }
        paint
    }

    /// Paint the frame: highlights, the overlay and command line, terminal grids, LSP state, then the
    /// render and, on a graphics terminal, the image pass.
    fn paint(&mut self, out: &mut io::Stdout, s: &mut Session, frame: &FrameCtx) -> io::Result<()> {
        let (revision, snapshot, rects) = (frame.revision, &frame.snapshot, &frame.rects);
        // Highlight only the VISIBLE byte range of the focused buffer (F-015 #3): the union of the
        // viewports of every pane showing it. The tree is reparsed incrementally on edit (keyed on
        // revision); the viewport-bounded query keeps the per-keystroke cost O(viewport), not O(buffer).
        let visible = visible_byte_range(&s.ed.ws, rects, &s.line_idx, snapshot.len());
        // Syntax highlighting is the FOCUSED buffer's own grammar (F-007): each file buffer has its own
        // highlighter in the registry; a scratch/no-file buffer has none, so it paints unhighlighted.
        let focused_id = s.ed.ws.focused_buffer();
        // F-031 3b-2b: reserve taller image blocks on a graphics-capable terminal (no-op after the first).
        if let Some(h) = s.highlighters.get_mut(&focused_id) {
            h.set_image_rows(self.image_rows);
        }
        let flags = s.status_flags();
        let focus_hl =
            s.focus_highlights(&mut self.cached_search, revision, snapshot, visible.clone());
        let overlay_rows = s.overlay_rows();
        // The syntax spans borrow the highlighter until the render, so they come after the two reads above.
        let (spans, virt_lines): (&[highlight::Span], &[highlight::VirtLine]) =
            match s.highlighters.get_mut(&focused_id) {
                Some(h) => h.spans_and_virt(revision, snapshot, visible.clone()),
                None => (&[], &[]),
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
        let term_views = term_views(&mut s.terminals, &s.ed.ws, rects);
        #[cfg(not(unix))]
        let term_views = crate::ui::render::TermViews::new();
        // F-014: sync the focused buffer with its server + poll all clients (diagnostics + response
        // dispatch) — the coordinator owns all of it; deferred edits/refs are applied after render.
        s.lsp
            .sync_and_poll(&s.ed.ws, &s.ed.files, revision, snapshot, &mut s.ed.status);
        let focus_diags = s.lsp.diagnostics_for(s.ed.ws.focused_buffer());
        // F-031 3b-2c: read each visible image's pixel dimensions (IHDR only) so render can size it to its
        // natural aspect and centre it. Cheap — only the 24-byte header, only when graphics are on.
        let image_dims = read_image_dims(self.has_graphics, virt_lines);
        // A mid-session recovery prompt (#859) cleared the terminal: repaint every cell this frame.
        if crate::ui::prompts::take_screen_clobbered() {
            self.prev_frame = screen::Screen::new(0, 0);
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
            out,
            &s.ed.ws,
            cmd_line.map(|(pfx, t)| (pfx, t, cmd_caret)),
            &cmd_menu,
            s.ed.status.text(),
            spans,
            virt_lines,
            rects,
            &frame.wins,
            &mut self.prev_frame,
            self.sync_output,
            &focus_hl,
            &overlay_rows,
            &term_views,
            focus_diags,
            s.lsp.completion_view(),
            self.has_graphics,
            &image_dims,
            focused_folds(&s.folds, &s.ed.ws),
            &s.line_idx,
            &s.pane_idx,
            s.ui_opts.showmode,
            s.ui_opts.conceal,
            &flags,
            s.ed.status.serial(),
            s.emacs_prompt.is_some() || s.changed_prompt.is_some(),
            &mut s.pager,
        )?;
        // F-031 slice 3b-2b: the graphics pass — after the cell flush, draw real pixels for the focused
        // pane's image blocks on a graphics-capable terminal (else the placeholder painted above stands).
        if self.has_graphics {
            let read_png = |path: &str| -> Option<Vec<u8>> {
                let meta = std::fs::metadata(path).ok()?;
                if meta.len() > (16 << 20) {
                    return None; // coarse decode-bomb / runaway guard (16 MiB)
                }
                let bytes = std::fs::read(path).ok()?;
                graphics::png_dimensions(&bytes).is_some().then_some(bytes)
            };
            graphics::graphics_pass(out, &images, &mut self.resident, self.in_tmux, read_png)?;
        }
        Ok(())
    }

    /// F-031 3b-2b: clear any inline images on exit so none is left drawn after the editor quits.
    fn clear_images(&self, out: &mut io::Stdout) {
        if self.has_graphics {
            use std::io::Write;
            let del = graphics::delete_all();
            let del = if self.in_tmux {
                graphics::wrap_tmux(&del)
            } else {
                del
            };
            let _ = out.write_all(&del);
            let _ = out.flush();
        }
    }
}

impl Session {
    /// The focused pane's extra reverse-video highlights: a `:s///c` confirm match, else the
    /// incsearch pattern being typed in `/`…`?`, else the last search (hlsearch) — F-009 #1. The
    /// search matches are viewport-cached (CachedSearch), not a per-frame full-buffer regex.
    fn focus_highlights(
        &self,
        cache: &mut highlight::CachedSearch,
        revision: ruse_core::Revision,
        snapshot: &[u8],
        visible: std::ops::Range<usize>,
    ) -> Vec<(usize, usize)> {
        if let Some(c) = &self.confirm {
            c.subs
                .get(c.idx)
                .map(|s| vec![(s.start.0, s.end.0)])
                .unwrap_or_default()
        } else {
            let active = match self.engine.cmdline() {
                // Incsearch: the pattern being typed, only when `incsearch` is on.
                Some(('/', buf, _)) | Some(('?', buf, _))
                    if self.incsearch_on && !buf.is_empty() =>
                {
                    Some(buf.to_string())
                }
                // Hlsearch: the last completed search, only when `hlsearch` is on.
                _ if self.hlsearch_on => self.search_hl.clone(),
                _ => None,
            };
            active
                .map(|p| {
                    cache
                        .spans_in(
                            self.ed.ws.focused_buffer(),
                            revision,
                            snapshot,
                            visible.clone(),
                            &p,
                        )
                        .to_vec()
                })
                .unwrap_or_default()
        }
    }

    /// Overlay match rows painted above the status line — at most one picker is open at a time, so all
    /// share the same uniform `rows()` paint slot (F-004 / F-013 NAT-3).
    fn overlay_rows(&self) -> Vec<(String, bool)> {
        if let Some(o) = self.overlay.as_ref() {
            o.rows()
        } else if self.engine.cmdwin().is_some() {
            // The command-line window (`q:`/`q/`/`q?`) lists the history ring in the same overlay slot.
            self.engine.cmdwin_rows()
        } else {
            // F-014: an LSP hover result shares the overlay slot (no picker can be open here).
            self.lsp.hover_overlay().unwrap_or_default()
        }
    }
}

/// F-011: reap terminals whose buffer was closed (Drop hangs up the child), resize each to its window
/// rect (so the child reflows), then pull pending PTY output into its VT grid so it paints this frame.
/// `term_views` lends each grid to the renderer. On non-unix this is always empty.
#[cfg(unix)]
fn term_views<'a>(
    terminals: &'a mut HashMap<DocumentId, term_buffer::Terminal>,
    ws: &Workspace,
    rects: &[Rect],
) -> crate::ui::render::TermViews<'a> {
    let live: std::collections::HashSet<DocumentId> = ws.buffers().iter().map(|b| b.id).collect();
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
}

/// Keys the session queued come before the terminal: the key half of a split Alt chord (#896), then
/// macro replay (D-055), so `@{reg}`'s decoded keys flow through the same dispatch as if re-typed. A
/// key from the replay queue is `from_replay`.
/// `None` when the event was no key (it is handled here) or the poll timed out.
fn next_key(s: &mut Session, frame: &FrameCtx) -> io::Result<Option<(KeyEvent, bool)>> {
    let key = match s.next_queued() {
        Some(queued) => queued,
        None => {
            // Input read ahead while a long command polled for CTRL-C comes first, in order.
            let ev = if let Some(ev) = crate::breakcheck::pop() {
                ev
            } else if let Some(ms) = s.poll_ms() {
                if event::poll(std::time::Duration::from_millis(ms))? {
                    event::read()?
                } else {
                    return Ok(None); // timed out: re-render freshly-drained async output
                }
            } else {
                event::read()?
            };
            match ev {
                Event::Key(k) => (k, false),
                // A bracketed paste (mode 2004) arrives as one event and never as keys (#600).
                Event::Paste(text) => {
                    s.on_paste(&text, frame);
                    return Ok(None);
                }
                // Focus gain looks at the files on disk again (RFC-0023 §8), as nvim's `:checktime`
                // on FocusGained. The guard asks for focus reports (mode 1004, #1220) where the
                // capability ledger has them.
                Event::FocusGained => {
                    s.check_disk(true);
                    return Ok(None);
                }
                _ => return Ok(None),
            }
        }
    };
    Ok(Some(key))
}

/// How often the event loop repaints while a macro replay runs (see `run`).
const REPLAY_PAINT: std::time::Duration = std::time::Duration::from_millis(100);

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
