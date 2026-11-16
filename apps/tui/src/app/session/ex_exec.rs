//! The bodies of the longer Ex commands `Session::execute_ex_parsed` dispatches (#1353): each one a
//! method that borrows the `Session` fields it uses, as the dispatcher did.
//!
//! This file keeps the line-range edits and listings that run one core `ExCmd` or print lines
//! (`:put`, `:z`, `:set`, `:mark`, `:>`/`:<`, `:d`, `:p`, `:retab`, `:j`, `:t`, `:y`, `:=`, `:only`).
//! The other families are children (#1500): `:s`/`:&` in [`substitute`], `:g` in [`global`], the
//! file and buffer commands in [`files`], the shell and terminal commands in [`shell`], the fold
//! commands in [`folds`], and the pickers and reports in [`listings`].

use super::*;
use ruse_core::{LineAddr, SubRange};

mod files;
mod folds;
mod global;
mod listings;
mod shell;
mod substitute;

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
