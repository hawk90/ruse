//! The bodies of the longer Ex commands `Session::execute_ex_parsed` dispatches (#1353): each one a
//! method that borrows the `Session` fields it uses, as the dispatcher did.

use super::*;
use crate::input::FileOpts;
use ruse_core::{LineAddr, SubRange};

impl Session {
    /// `:[line]put [reg]` — put a register's text LINEWISE as new whole line(s) after the addressed
    /// line (a charwise register is still put as whole lines — the linewise-forcing rule).
    pub(super) fn ex_put(&mut self, addr: LineAddr, reg: Option<char>, above: bool) {
        let Session {
            ws,
            files,
            recorded,
            quit,
            ui_opts,
            status,
            ..
        } = self;
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

    /// `:z` (#609): a window of lines around the addressed one; the cursor goes to the line nvim's
    /// `ex_z` picks, at column 0 when that is another line.
    pub(super) fn ex_z(&mut self, spec: crate::input::ZSpec, frame: &FrameCtx) {
        let Session {
            ws,
            recorded,
            overlay,
            status,
            ex_filter,
            ..
        } = self;
        let cols = frame.cols;
        let text_rows = frame.text_rows;
        let rects = &frame.rects;
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
                crate::app::dispatch::resolve_normal_range(ws, ruse_core::SubRange::CurrentLine).1,
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

    /// `:set` / `:setlocal` / `:setglobal` (#614): the grammar is `input/set_cmd.rs`, the values it
    /// reads and writes are behind `set_exec::OptionHost`.
    pub(super) fn ex_set(&mut self, line: crate::input::set_cmd::SetLine, frame: &FrameCtx) {
        let Session {
            ws,
            files,
            recorded,
            engine,
            folds,
            fold_method,
            foldlevel,
            foldenable,
            fold_rev,
            search_hl,
            hlsearch_on,
            incsearch_on,
            scrolloff,
            ui_opts,
            opt_sources,
            ex_verbose,
            overlay,
            status,
            readonly,
            ..
        } = self;
        let cols = frame.cols;
        let win_height = frame.rects.get(ws.focus()).map_or(0, |r| usize::from(r.h));
        let mut host = crate::app::set_exec::OptionHost {
            ws,
            recorded,
            engine,
            hlsearch_on,
            search_hl,
            incsearch_on,
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
        let out = crate::app::set_exec::run_set(&line, &mut host, cols as usize, *ex_verbose);
        status.history_mut().set_cap(ui_opts.message_history());
        crate::app::ex_more::show_rows(out.rows, status, overlay);
        if let Some(e) = out.error {
            status.error(e);
        }
    }

    /// `:[range]mark {x}` / `:[range]k{x}` (#1088).
    pub(super) fn ex_mark(&mut self, range: SubRange, name: char) {
        let Session { ws, recorded, .. } = self;
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

    /// `:checkhealth` (F-030): gather the frontend snapshot (guard/profile/highlighter) and render
    /// the report's one-line summary into status.
    pub(super) fn ex_checkhealth(&mut self) {
        let Session {
            ws,
            files,
            highlighters,
            recorded,
            profile_name,
            emacs_profile,
            term,
            config,
            overlay,
            status,
            rescue,
            journals,
            journal_writers,
            ..
        } = self;
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
        crate::app::ex_more::show_rows(health::lines(&health::report(&inputs)), status, overlay);
    }

    /// `:[addr]r {file}` / `:[addr]r !{cmd}` — read a file's contents (pure IO) or a shell
    /// command's stdout below the addressed line. The read is IMPURE (fs / process) and stays in
    /// the frontend; core only splices the resulting bytes. The cursor lands on the
    /// FIRST inserted line for a file read, the LAST for a command read (nvim v0.12.4 quirk).
    pub(super) fn ex_read(&mut self, addr: LineAddr, source: ReadSource) {
        let Session {
            ws,
            recorded,
            status,
            ..
        } = self;
        match source {
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
        }
    }

    /// `:{range}!{cmd}` — FILTER: pipe the range's lines through the shell command and replace
    /// them with its output. Pull the range text out FIRST (read-only), shell out, then splice.
    /// As in nvim the output (stdout + stderr) replaces the range even when the command fails,
    /// and the status line says `shell returned {n}` (#861).
    pub(super) fn ex_filter(&mut self, range: SubRange, cmd: String) {
        let Session {
            ws,
            files,
            recorded,
            status,
            ..
        } = self;
        match ws.range_text(range) {
            Some(input) => match crate::shell::filter(&cmd, {
                // The range's last line goes out as the file ends it (nvim: `:%!od -c` of `a\nb`
                // without `'eol'` reads three bytes, RFC-0024 §3).
                if crate::app::file_cmds::reaches_last_line(ws, range) {
                    let fmt = crate::app::file_cmds::buffer_format(files, ws.focused_buffer());
                    files.write.lines_out(&input, fmt)
                } else {
                    &input
                }
            }) {
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
        }
    }

    /// `:savetrace`.
    pub(super) fn ex_save_trace(&mut self, p: String) {
        let Session {
            ws,
            files,
            initial,
            start_options,
            start_registers,
            trace_watch,
            recorded,
            status,
            ..
        } = self;
        let trace =
            Trace::record_with_options(initial, start_options, start_registers, recorded.to_vec());
        // A trace that would not replay to this document is not written (#690). The trace holds
        // register contents and pasted clipboard text as well as the edits (RFC-0020 R6, Security
        // impact), so the message says so.
        let checked = crate::app::trace_check::check(&trace, initial, ws, files, trace_watch)
            .map_err(|r| r.to_string());
        match checked.and_then(|()| {
            std::fs::write(&p, trace.to_text()).map_err(|e| format!("trace save failed: {e}"))
        }) {
            Ok(()) => status.info(format!(
                "trace saved: {p} ({} commands; includes registers and clipboard text)",
                recorded.len()
            )),
            Err(e) => status.error(e),
        }
    }

    /// `:[range]>` / `:[range]<` — shift the range's lines one indent level per repeated verb char,
    /// reusing the core `>>`/`<<` shift (no range = the current line).
    pub(super) fn ex_shift(&mut self, range: SubRange, left: bool, levels: u32) {
        let Session {
            ws,
            recorded,
            folds,
            ui_opts,
            status,
            ..
        } = self;
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

    /// Bare `:s` / `:s {flags}` / `:&` / `:&&` — repeat the last `:s` against the frontend's
    /// substitute history (the core keeps none), exactly like `&`/`g&`. `flags = None` (`:&&`)
    /// KEEPS the stored flags; `flags = Some(f)` REPLACES them (bare `:s`/`:&` = default flags,
    /// `:s g` = the given flags). Verified vs nvim v0.12.4.
    pub(super) fn ex_repeat_substitute(
        &mut self,
        range: SubRange,
        flags: Option<ruse_core::SubFlags>,
    ) {
        let Session {
            ws,
            recorded,
            ui_opts,
            last_substitute,
            status,
            global_busy,
            ..
        } = self;
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
                    Ok(out) => crate::app::dispatch::substituted_message(&out, ui_opts.report),
                    Err(e) => Msg::error(crate::app::dispatch::regex_error_msg(&e)),
                }
            }
            None => Msg::error("E33: No previous substitute regular expression"),
        });
    }

    /// `:[range]d` — delete the range's lines (no range = the current line) as one undo group.
    /// `:d`, `:>` and `:<` are jumps from where the cursor stood (nvim's `ex_operators`, #1119).
    pub(super) fn ex_delete(
        &mut self,
        range: SubRange,
        reg: Option<char>,
        flags: Option<crate::input::PrintFlags>,
    ) {
        let Session {
            ws,
            recorded,
            ui_opts,
            status,
            ..
        } = self;
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

    /// `:terminal` (F-011): spawn a shell in a new PTY-backed buffer, sized to the focused
    /// window, and enter Terminal mode. Unix-only in slice 1.
    pub(super) fn ex_terminal(&mut self, frame: &FrameCtx) {
        let Session {
            ws,
            files,
            recorded,
            quit,
            terminals,
            status,
            ..
        } = self;
        let cols = frame.cols;
        let text_rows = frame.text_rows;
        let rects = &frame.rects;
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

    /// `:p`, `:l`, `:nu`, `:#` (#609): the range's lines in the message area, filtered by a `:filter`;
    /// the cursor goes to the last line of the range at the wanted column, as `:N` does.
    pub(super) fn ex_print(&mut self, range: SubRange, flags: crate::input::PrintFlags) {
        let Session {
            ws,
            files,
            recorded,
            quit,
            overlay,
            status,
            ex_filter,
            ..
        } = self;
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

    /// `:saveas`.
    pub(super) fn ex_saveas(&mut self, bang: bool, file: String, opts: FileOpts) {
        let Session {
            ws,
            files,
            highlighters,
            status,
            alt_file,
            ..
        } = self;
        let id = ws.focused_buffer();
        let old = files.get(&id).map(|bf| bf.path.display().to_string());
        let ok =
            crate::app::file_cmds::ex_saveas(bang, &file, opts, ws, files, highlighters, status);
        // The buffer's old name becomes the alternate file (Vim's 'cpoptions' `A`, #610).
        let renamed = files.get(&id).map(|bf| bf.path.display().to_string());
        if let Some(old) = old.filter(|o| ok && Some(o) != renamed.as_ref()) {
            *alt_file = Some(crate::app::ex_more::AltFile::new(ws, old));
        }
    }

    /// `:bd` deletes the focused buffer and drops its `files`/highlighter entries. Guards unsaved
    /// changes with E89 unless `!` forces it.
    pub(super) fn ex_bdelete(&mut self, force: bool) {
        let Session {
            ws,
            files,
            highlighters,
            status,
            ..
        } = self;
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

    /// The `:w` family (#610). `:wq`/`:x` close the window only after a successful write (#842): no
    /// file, or a failed write, keeps it open; `!` also lets the focused buffer's changes go.
    pub(super) fn ex_write(&mut self, spec: WriteSpec) {
        let Session {
            ws,
            files,
            highlighters,
            quit,
            status,
            alt_file,
            ..
        } = self;
        let id = ws.focused_buffer();
        let had_file = files.contains_key(&id);
        let ok = crate::app::file_cmds::ex_write(&spec, ws, files, highlighters, status);
        // Writing a buffer that has a file to another name makes that name the alternate file
        // (Vim's 'cpoptions' `A`, #610).
        if let (true, true, WriteTarget::File(Some(name))) = (ok, had_file, &spec.target) {
            let other = crate::app::dispatch::buffer_for_path(files, std::path::Path::new(name));
            if other != Some(id) {
                *alt_file = Some(crate::app::ex_more::AltFile::new(ws, name.clone()));
            }
        }
        if ok && spec.quit {
            quit_window(ws, status, quit, spec.bang);
        }
    }

    /// `:!{cmd}` — run the command and show its output on the status line; NO buffer change.
    /// Multi-line output is collapsed to one status line (a scrolling message pane is a
    /// follow-up); an empty result echoes the command so the user sees it ran.
    pub(super) fn ex_shell(&mut self, cmd: String) {
        let Session { status, .. } = self;
        match crate::shell::capture(&cmd) {
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
        }
    }

    /// `:retab` (#609).
    pub(super) fn ex_retab(
        &mut self,
        range: SubRange,
        bang: bool,
        tabstop: Option<Box<ruse_core::VarStops>>,
    ) {
        let Session { ws, recorded, .. } = self;
        let (first, last) = ws.resolve_range(range);
        let ex = ExCmd::Retab {
            first,
            last,
            bang,
            tabstop: tabstop.map(|v| *v),
        };
        run_ex(ex, ws, recorded);
    }

    /// `:[range]g/pat/cmd` (F-009 #4): two-pass mark-then-execute over the focused window. A bare `d`
    /// and a plain `s` run in core; a `normal` payload replays keys per marked line through the
    /// engine, and any other command runs per marked line through this dispatch (#1245).
    /// An empty `:g//` pattern is the last search, and the `:g` pattern becomes the last search
    /// before the command runs, so `:g/pat/s//X/` substitutes `pat` (Vim, #804).
    pub(super) fn ex_global(&mut self, mut ex: Ex, frame: &FrameCtx) {
        let Session {
            ws,
            recorded,
            engine,
            search_hl,
            ui_opts,
            overlay,
            status,
            ex_filter,
            global_busy,
            ..
        } = self;
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
                ruse_core::SubRange::Lines(1, n) => n == crate::app::dispatch::line_count(b).max(1),
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
                let global_subs = match cmd {
                    ruse_core::GlobalCmd::Substitute { pattern, flags, .. } if flags.global => {
                        let span = ruse_core::SubRange::Lines(
                            marked.first().copied().unwrap_or(1),
                            last_marked,
                        );
                        let on: HashSet<usize> = marked.iter().map(|m| m - 1).collect();
                        ws.substitute_preview(span, pattern, "", *flags)
                            .ok()
                            .map(|subs| subs.iter().filter(|s| on.contains(&s.line)).count())
                    }
                    _ => None,
                };
                // Recorded as the lines the pattern marked and the command (RFC-0020).
                let ex = ExCmd::Global {
                    marks: marked,
                    cmd: cmd.clone(),
                };
                let lines_before = crate::app::dispatch::line_count(ws.focused().doc.bytes());
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
                        let at =
                            crate::app::ex_more::line_start(ws.focused().doc.bytes(), last_marked);
                        place_cursor(ws, recorded, at);
                        status.clear();
                    }
                    // nvim's `:g` report: the lines `:d` removed (`msgmore`), or the substitutions
                    // the `:s` made.
                    Ok(n) => match cmd {
                        ruse_core::GlobalCmd::Delete if ws.focused().doc.bytes().is_empty() => {
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
                            report::substituted(global_subs.unwrap_or(n), n, ui_opts.report),
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

    /// `:[range]s/pat/rep/flags` (F-009 #2). An empty pattern reuses the last search (`/`/`*`), as
    /// Vim does, and a non-empty one is remembered so `&` can repeat it. It also becomes the last
    /// search pattern (Vim's `save_re_pat`): `n` finds it next, and `:g//` or `:s//` reuse it.
    pub(super) fn ex_substitute(&mut self, mut ex: Ex) {
        let Session {
            ws,
            recorded,
            engine,
            confirm,
            search_hl,
            ui_opts,
            last_substitute,
            status,
            global_busy,
            ..
        } = self;
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
            spec.range = ruse_core::SubRange::Lines(line2, line2.saturating_add(n - 1).min(total));
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
            *last_substitute = Some((spec.pattern.clone(), spec.replacement.clone(), flags));
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
            match ws.substitute_preview(spec.range, &spec.pattern, &spec.replacement, flags) {
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
                        crate::app::ex_print::rows(b, line, line, pflags, pane.view.tabs(), &|_| {
                            true
                        })
                        .pop()
                        .map_or_else(|| Msg::echo(""), Msg::echo)
                    }
                    None => crate::app::dispatch::substituted_message(&out, ui_opts.report),
                },
                Err(e) => Msg::error(crate::app::dispatch::invalid_command_msg(&e)),
            });
        }
    }

    /// `:inspect config [key]` (CFG-PROV-001): the layers, their state, every value a layer set
    /// with its source, and the diagnostics; or one key's layer stack.
    pub(super) fn ex_inspect_config(&mut self, key: Option<String>) {
        let Session {
            config,
            overlay,
            status,
            ..
        } = self;
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

    /// `:{range}foldopen[!]` / `:{range}foldclose[!]` (#1141): `zo`/`zc` (`zO`/`zC`) on each line of
    /// the range, past each closed fold; `E490` when no fold holds any of its lines.
    pub(super) fn ex_fold_open(&mut self, range: SubRange, open: bool, recursive: bool) {
        let Session {
            ws, folds, status, ..
        } = self;
        let (a, b) = ws.resolve_range(range);
        let fv = folds.entry(ws.focused_buffer()).or_default();
        if !crate::folds::op_range(fv, a.max(1) - 1, b.max(1) - 1, open, recursive) {
            status.error("E490: No fold found");
        }
    }

    /// `:marks` (F-003): open a picker over the set marks; Enter jumps to the selected one.
    pub(super) fn ex_marks(&mut self) {
        let Session {
            ws,
            overlay,
            status,
            ..
        } = self;
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

    /// `:[range]j[oin][!]` — join the range's lines into one, reusing the core `J`/`gJ` join (no range =
    /// the current line, which joins with the next). `bang` is the raw `gJ` form.
    pub(super) fn ex_join(&mut self, range: SubRange, bang: bool) {
        let Session {
            ws,
            recorded,
            status,
            ..
        } = self;
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

    /// `:{range}folddoopen {cmd}` / `:{range}folddoclosed {cmd}` (#1141): run `{cmd}` on each line of
    /// the range not in (or in) a closed fold, through the per-line dispatch `:g` uses (nvim's
    /// `ex_folddo` marks the lines, then runs `global_exe`).
    pub(super) fn ex_fold_do(
        &mut self,
        range: SubRange,
        closed: bool,
        cmd: String,
        frame: &FrameCtx,
    ) {
        let Session { ws, folds, .. } = self;
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

    /// `:{range}fold` (#1141): a closed manual fold over the range, as `zf`; the cursor stays.
    pub(super) fn ex_fold(&mut self, range: SubRange) {
        let Session {
            ws,
            folds,
            fold_method,
            status,
            ..
        } = self;
        let buf = ws.focused_buffer();
        if fold_method.get(&buf).copied() == Some(crate::input::FoldMethod::Indent) {
            status.error(E350);
            return;
        }
        let (a, b) = ws.resolve_range(range);
        let fv = folds.entry(buf).or_default();
        crate::folds::insert_closed_fold(fv, a.max(1) - 1, b.max(1) - 1);
    }

    /// `:diagnostics` (F-014): open a picker over the focused buffer's already-collected
    /// diagnostics (no server round-trip); Enter jumps to the selected one.
    pub(super) fn ex_diagnostics(&mut self, frame: &FrameCtx) {
        let Session {
            ws,
            overlay,
            lsp,
            status,
            ..
        } = self;
        let snapshot = &frame.snapshot;
        let diags = lsp.diagnostics_for(ws.focused_buffer());
        if diags.is_empty() {
            status.echo("no diagnostics");
        } else {
            status.echo(format!("{} diagnostic(s)", diags.len()));
            *overlay = Some(Overlay::Diagnostics(diag_picker::open(diags, snapshot)));
        }
    }

    /// `:registers` (F-029): open a view-only picker over the non-empty registers, so a
    /// recorded macro (`"a`) or a yank is inspectable.
    pub(super) fn ex_registers(&mut self) {
        let Session {
            ws,
            overlay,
            status,
            ..
        } = self;
        let snapshot = ws.register_snapshot();
        if snapshot.is_empty() {
            status.echo("no registers set");
        } else {
            status.echo(format!("{} register(s)", snapshot.len()));
            *overlay = Some(Overlay::Registers(register_picker::open(snapshot)));
        }
    }

    /// `:[range]t {addr}` / `:copy` — copy the range's lines to after the destination line.
    pub(super) fn ex_copy(&mut self, range: SubRange, dest: LineAddr) {
        let Session {
            ws,
            recorded,
            ui_opts,
            status,
            ..
        } = self;
        let ((first, last), dest) = (ws.resolve_range(range), ws.resolve_addr(dest));
        let copied =
            dest.and_then(
                |dest| match run_ex(ExCmd::Copy { first, last, dest }, ws, recorded) {
                    ExOutcome::Relocated(n) => n,
                    _ => None,
                },
            );
        match copied {
            Some(n) => give_report(status, report::more(to_i64(n), ui_opts.report)),
            None => status.error("E486: invalid copy"),
        }
    }

    /// `:file` (#610): show the file information, rename the buffer, or remove its name. The old name
    /// becomes the alternate file.
    pub(super) fn ex_file(&mut self, cmd: crate::input::FileCmd) {
        let Session {
            ws,
            files,
            highlighters,
            status,
            alt_file,
            ..
        } = self;
        let id = ws.focused_buffer();
        let old = files.get(&id).map(|bf| bf.path.display().to_string());
        match cmd {
            crate::input::FileCmd::Info => {
                if let Some(msg) = info_status(&Command::FileInfo, ws, true) {
                    status.give(msg);
                }
            }
            // The name is expanded with the other file arguments above.
            crate::input::FileCmd::Rename(name) => {
                let fmt = files.get(&id).map(|bf| bf.fmt).unwrap_or_default();
                crate::app::file_cmds::adopt_file(ws, files, highlighters, id, &name, fmt);
                if let Some(old) = old.filter(|o| *o != name) {
                    *alt_file = Some(crate::app::ex_more::AltFile::new(ws, old));
                }
            }
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

    /// The directory is expanded with the other file arguments above.
    pub(super) fn ex_cd(&mut self, dir: Option<String>) {
        let Session {
            ws,
            files,
            status,
            prev_dir,
            ..
        } = self;
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

    /// `:find` / `:sfind` (#610): edit the file found in `'path'` (`.,,`), in a new window for `:sf`.
    pub(super) fn ex_find(&mut self, split: bool, spec: EditSpec) -> Option<String> {
        let Session {
            ws,
            files,
            highlighters,
            status,
            ..
        } = self;
        let mut then_run = None;
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
        then_run
    }

    /// `:drop {file}` (#610): go to a window that shows the file, or edit it here.
    pub(super) fn ex_drop(&mut self, spec: EditSpec) -> Option<String> {
        let Session {
            ws,
            files,
            highlighters,
            status,
            ..
        } = self;
        let mut then_run = None;
        let name = spec.file.clone().unwrap_or_default();
        let shown = crate::app::dispatch::buffer_for_path(files, std::path::Path::new(&name))
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
        then_run
    }

    /// `:e[!] [++ff] [+cmd] [{file}]` (F-007, #610): open a file into a buffer (shared with the file
    /// picker), or re-read the focused buffer's own file, which unsaved changes block (`E37`) without `!`.
    /// A `+cmd` runs once the file is open (after this dispatch, which holds the session's fields).
    pub(super) fn ex_edit(&mut self, spec: EditSpec) -> Option<String> {
        let Session {
            ws,
            files,
            highlighters,
            status,
            ..
        } = self;
        let mut then_run = None;
        if crate::app::file_cmds::ex_edit(&spec, ws, files, highlighters, status) {
            then_run = spec
                .cmd
                .as_deref()
                .map(crate::app::file_cmds::plus_cmd_line);
        }
        then_run
    }

    /// `:[range]y` — yank the range's lines linewise into the unnamed register (like `yy`).
    pub(super) fn ex_yank(&mut self, range: SubRange, reg: Option<char>) {
        let Session {
            ws,
            recorded,
            ui_opts,
            status,
            ..
        } = self;
        let (first, last) = ws.resolve_range(range);
        let n = lines_outcome(&run_ex(ExCmd::Yank { first, last, reg }, ws, recorded));
        give_report(status, report::yanked(n, false, reg, ui_opts.report));
    }

    /// `:=` (#609): the last line number of the range, or of the buffer.
    pub(super) fn ex_line_number(&mut self, range: Option<SubRange>) {
        let Session { ws, status, .. } = self;
        let n = match range {
            Some(r) => crate::app::dispatch::resolve_normal_range(ws, r).1,
            None => crate::app::dispatch::line_count(ws.focused().doc.bytes()),
        };
        status.info(n.to_string());
    }

    /// `:only`.
    pub(super) fn ex_only(&mut self) {
        let Session { ws, status, .. } = self;
        status.echo(match ws.only() {
            0 => "already one window".into(),
            1 => "1 window closed".into(),
            n => format!("{n} windows closed"),
        });
    }
}
