//! `:g` (#1500): mark the lines, then run the payload on them: in core (`d`, `s`), as lines to
//! print, as Normal keys, or as another Ex command per line. [`Session::ex_global`] resolves the
//! range and marks the lines; each payload kind has its own method.

use std::collections::HashSet;

use ruse_core::{ExCmd, ExOutcome};

use crate::app::dispatch::{lines_outcome, place_cursor, regex_error_msg, run_ex};
use crate::app::report;
use crate::app::session::{give_report, FrameCtx, Session};
use crate::input::Ex;

impl Session {
    /// `:[range]g/pat/cmd` (F-009 #4): two-pass mark-then-execute over the focused window. A bare `d`
    /// and a plain `s` run in core; a `normal` payload replays keys per marked line through the
    /// engine, and any other command runs per marked line through this dispatch (#1245).
    /// An empty `:g//` pattern is the last search, and the `:g` pattern becomes the last search
    /// before the command runs, so `:g/pat/s//X/` substitutes `pat` (Vim, #804).
    pub(in crate::app::session) fn ex_global(&mut self, mut ex: Ex, frame: &FrameCtx) {
        let Session {
            ws,
            engine,
            search_hl,
            status,
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
            crate::input::GlobalPayload::Core(cmd) => self.ex_global_core(marked, last_marked, cmd),
            // `:g/pat/p` (#609): show the marked lines; the cursor ends on the last one.
            crate::input::GlobalPayload::Print(flags) => {
                self.ex_global_print(&marked, last_marked, *flags);
            }
            crate::input::GlobalPayload::Normal { keys, .. } => {
                self.ex_global_normal(&marked, keys, frame);
            }
            crate::input::GlobalPayload::Ex(cmd) => self.ex_global_ex(&marked, cmd, frame),
        }
    }

    /// A `:g` payload that runs in core (`d` or a plain `s`) over the `marked` lines, the last of
    /// which is `last_marked`, and its report.
    fn ex_global_core(
        &mut self,
        marked: Vec<usize>,
        last_marked: usize,
        cmd: &ruse_core::GlobalCmd,
    ) {
        let Session {
            ws,
            recorded,
            engine,
            search_hl,
            ui_opts,
            status,
            ..
        } = self;
        // A `:g/pat/s///g` reports its substitutions (`4 substitutions on 2 lines`): the
        // matches on the marked lines, counted before it runs. Without `g` it is one per
        // line the `:s` changed.
        let global_subs = match cmd {
            ruse_core::GlobalCmd::Substitute { pattern, flags, .. } if flags.global => {
                let span =
                    ruse_core::SubRange::Lines(marked.first().copied().unwrap_or(1), last_marked);
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
                let at = crate::app::ex_more::line_start(ws.focused().doc.bytes(), last_marked);
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

    /// `:g/pat/p` (#609): the `marked` lines in the message area, filtered by a `:filter`; the
    /// cursor ends at the start of `last_marked`.
    fn ex_global_print(
        &mut self,
        marked: &[usize],
        last_marked: usize,
        flags: crate::input::PrintFlags,
    ) {
        let Session {
            ws,
            recorded,
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
        let pane = ws.focused();
        let b = pane.doc.bytes();
        let ts = pane.view.tabs();
        let rows: Vec<String> = marked
            .iter()
            .flat_map(|&n| crate::app::ex_print::rows(b, n, n, flags, ts, &*keep))
            .collect();
        let at = crate::app::ex_more::line_start(b, last_marked);
        place_cursor(ws, recorded, at);
        crate::app::ex_more::show_rows(rows, status, overlay);
    }
}
