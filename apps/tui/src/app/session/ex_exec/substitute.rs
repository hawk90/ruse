//! `:s` and its repeats `:&`/`:&&`/bare `:s` (#1500). [`Session::ex_substitute`] resolves the
//! range, the flags `&` keeps and the last-substitute and last-search state; then the `:s` ends
//! one of three ways, each its own method: the `n` count, the `c` confirm loop, or the edit itself.

use ruse_core::{SubFlags, SubRange};

use crate::app::dispatch::{run_ex, sub_outcome, substitute_cmd};
use crate::app::messages::Msg;
// `match_count_message` comes through the session's own imports; this module is its only user.
use crate::app::session::{match_count_message, Editing, Session};
use crate::input::{Ex, PrintFlags};
use crate::ui::prompts::Confirm;

/// A `:s` as [`Session::ex_substitute`] resolved it (range, pattern, flags), for the method that
/// ends it.
struct SubRun<'a> {
    range: SubRange,
    pattern: &'a str,
    replacement: &'a str,
    flags: SubFlags,
    /// `e`: no `E486` when nothing matches.
    quiet: bool,
    /// `p`, `#` or `l`: show the last substituted line.
    print: Option<PrintFlags>,
}

impl Session {
    /// Bare `:s` / `:s {flags}` / `:&` / `:&&` — repeat the last `:s` against the frontend's
    /// substitute history (the core keeps none), exactly like `&`/`g&`. `flags = None` (`:&&`)
    /// KEEPS the stored flags; `flags = Some(f)` REPLACES them (bare `:s`/`:&` = default flags,
    /// `:s g` = the given flags). Verified vs nvim v0.12.4.
    pub(in crate::app::session) fn ex_repeat_substitute(
        &mut self,
        range: SubRange,
        flags: Option<ruse_core::SubFlags>,
    ) {
        let Session {
            ed:
                Editing {
                    ws,
                    recorded,
                    status,
                    ..
                },
            ui_opts,
            last_substitute,
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

    /// `:[range]s/pat/rep/flags` (F-009 #2). An empty pattern reuses the last search (`/`/`*`), as
    /// Vim does, and a non-empty one is remembered so `&` can repeat it. It also becomes the last
    /// search pattern (Vim's `save_re_pat`): `n` finds it next, and `:g//` or `:s//` reuse it.
    pub(in crate::app::session) fn ex_substitute(&mut self, mut ex: Ex) {
        let Session {
            ed: Editing { ws, status, .. },
            engine,
            search_hl,
            last_substitute,
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
        let run = SubRun {
            range: spec.range,
            pattern: &spec.pattern,
            replacement: &spec.replacement,
            flags,
            quiet: spec.quiet,
            print: spec.print,
        };
        if spec.count_only {
            self.substitute_count_only(&run);
        } else if spec.confirm {
            self.substitute_confirm(&run);
        } else {
            self.substitute_apply(&run);
        }
    }

    /// `:s///n`: REPORT ONLY — count the matches and echo the tally, editing nothing (Vim `:s///n`).
    /// Overrides `c` (like Vim). `substitute_count` borrows `&`, so no edit / undo / cursor move.
    fn substitute_count_only(&mut self, run: &SubRun<'_>) {
        let Session {
            ed: Editing { ws, status, .. },
            global_busy,
            ..
        } = self;
        status.give(
            match ws.substitute_count(run.range, run.pattern, run.flags) {
                Ok(out) if out.replacements == 0 && *global_busy => Msg::echo(""),
                Ok(out) if out.replacements == 0 => {
                    Msg::error(format!("E486: Pattern not found: {}", run.pattern))
                }
                Ok(out) => Msg::info(match_count_message(out.replacements, out.lines)),
                Err(e) => Msg::error(crate::app::dispatch::invalid_command_msg(&e)),
            },
        );
    }

    /// `:s///c`: compute the matches and hand control to the interactive confirm loop (F-009 #2).
    fn substitute_confirm(&mut self, run: &SubRun<'_>) {
        let Session {
            ed: Editing { ws, status, .. },
            confirm,
            ..
        } = self;
        match ws.substitute_preview(run.range, run.pattern, run.replacement, run.flags) {
            Ok(subs) if subs.is_empty() => {
                status.error(format!("E486: Pattern not found: {}", run.pattern));
            }
            Ok(subs) => {
                // The confirm loop records the resolved `:s` with the answers when it ends;
                // that pushes the jump from where the cursor is now (nvim's `do_sub`, #1119).
                let ex = substitute_cmd(
                    ws,
                    run.range,
                    run.pattern,
                    run.replacement,
                    run.flags,
                    Some(Vec::new()),
                );
                let from = ws.focused().view.cursor().0;
                *confirm = Some(Confirm::new(subs, ex, from));
            }
            Err(e) => status.error(crate::app::dispatch::invalid_command_msg(&e)),
        }
    }

    /// A plain `:s`: the edit, and its report (or the substituted line under `p`/`#`/`l`).
    fn substitute_apply(&mut self, run: &SubRun<'_>) {
        let Session {
            ed:
                Editing {
                    ws,
                    recorded,
                    status,
                    ..
                },
            ui_opts,
            global_busy,
            ..
        } = self;
        // Recorded resolved (RFC-0020); a `:s` that matched is a jump (#940).
        let ex = substitute_cmd(ws, run.range, run.pattern, run.replacement, run.flags, None);
        status.give(match sub_outcome(run_ex(ex, ws, recorded)) {
            // Nor inside a `:g`, which says nothing for the lines `:s` misses (Vim's `do_sub`).
            Ok(out) if out.lines == 0 && (run.quiet || *global_busy) => Msg::echo(""),
            Ok(out) if out.lines == 0 => {
                Msg::error(format!("E486: Pattern not found: {}", run.pattern))
            }
            // A `:s/\n//` join on the last line: nothing to join, not an error.
            Ok(out) if out.replacements == 0 => Msg::echo(""),
            Ok(out) => match run.print {
                // `p`/`#`/`l`: show the last substituted line, where the cursor is.
                Some(pflags) => {
                    let pane = ws.focused();
                    let b = pane.doc.bytes();
                    let line = ruse_core::pos::line_of(b, pane.view.cursor()) + 1;
                    let uhex = ui_opts.display.uhex();
                    let ts = pane.view.tabs();
                    crate::app::ex_print::rows(b, line, line, pflags, ts, uhex, &|_| true)
                        .pop()
                        .map_or_else(|| Msg::echo(""), Msg::echo)
                }
                None => crate::app::dispatch::substituted_message(&out, ui_opts.report),
            },
            Err(e) => Msg::error(crate::app::dispatch::invalid_command_msg(&e)),
        });
    }
}
