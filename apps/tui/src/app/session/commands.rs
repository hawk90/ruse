//! Running a finished command the way a typed key runs (#689): the display-only commands, the search
//! resolution and the dispatch, and the free helpers they use. Split from `session.rs` (#1423).

use super::{
    highlight, indent, line_index, regex_error_msg, run_ex, search_pattern, sub_outcome,
    substitute_cmd, sync_special_registers, Command, DocumentId, Editing, Files, FrameCtx, HashMap,
    InputEngine, Msg, OpenKind, Overlay, Session, Status, Workspace, E35,
};

impl Session {
    /// Run a finished engine `Command` the way a typed key's is run (#689): the frontend-resolved ones
    /// (`&`/`g&`, the INFO commands, `[i`/`[I`, `*`/`#`, a missed search, `gd`, tree-aware `=`) are resolved
    /// here, hlsearch and the special registers are refreshed, then it is recorded and applied. The
    /// command palette dispatches through this too, so a command picked there behaves as when typed.
    pub(super) fn run_resolved_cmd(&mut self, cmd: Command, frame: &FrameCtx) {
        // The Emacs profile's Quit is `save-buffers-kill-terminal`, which asks (#619).
        if self.emacs_profile && cmd == Command::Quit {
            self.emacs_action(crate::input::EmacsAction::KillTerminal, frame);
            return;
        }
        if self.display_cmd(&cmd) {
            return;
        }
        let Some(cmd) = self.resolve_search(cmd) else {
            return;
        };
        self.dispatch_resolved(cmd, frame);
    }

    /// The commands the session answers on the status line or in an overlay, never planned: `g&`, `&`
    /// picked from the palette, the INFO commands and `[i`/`[I`. Whether `cmd` was one.
    pub(super) fn display_cmd(&mut self, cmd: &Command) -> bool {
        let report = self.ui_opts.report;
        let Session {
            ed:
                Editing {
                    ws,
                    files,
                    recorded,
                    status,
                    ..
                },
            last_substitute,
            ..
        } = self;
        // `g&` — repeat the last `:s` over the WHOLE FILE with its flags. Resolved here (the engine
        // has no substitute history); like the current-line `&`, it calls `substitute` directly.
        if matches!(*cmd, Command::RepeatSubstituteGlobal) {
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
            return true;
        }
        // `&` picked by its command id (the palette): the typed `&` with no count.
        if matches!(*cmd, Command::RepeatSubstituteLine) {
            status.give(repeat_substitute_lines(
                ws,
                recorded,
                last_substitute.as_ref(),
                1,
                report,
            ));
            return true;
        }
        // INFO commands (`ga`/`:ascii`, `CTRL-G`, `g CTRL-G`): print to the status line, no buffer
        // mutation. Like `*`/`gd`, the engine has no buffer, so the frontend reads the focused
        // buffer's bytes + cursor here and formats the message via the pure `ruse_core::info` helpers.
        if let Some(msg) = info_status(cmd, ws, writes_final_newline(files, ws)) {
            status.give(msg);
            return true;
        }
        // `[i`/`]i`/`[I`/`]I` (keyword under cursor): the engine has no buffer, so resolve the
        // keyword here and scan the buffer lines. `[i`/`]i` echo the count'th match on the status
        // line; `[I`/`]I` open a view-only listing overlay. Purely a display command (never planned).
        // No keyword under the cursor → `E349` (like nvim; note `*`/`gd` use `E348`, a different code).
        if let Command::ShowKeywordLines { above, list, count } = *cmd {
            self.show_keyword_lines(above, list, count);
            return true;
        }
        false
    }

    /// `[i`/`]i` echo the `count`'th line holding the keyword under the cursor; `[I`/`]I` (`list`) open a
    /// view-only listing of them. `E349` with no keyword under the cursor.
    pub(super) fn show_keyword_lines(&mut self, above: bool, list: bool, count: u32) {
        let Session {
            ed: Editing { ws, status, .. },
            overlay,
            ..
        } = self;
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
                        match ruse_core::keyword_echo(&matches, cursor_line, above, count as usize)
                        {
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
    }

    /// Resolve a search before it runs: `*`/`#` record their pattern, a search that matches nowhere or has
    /// no previous pattern fails here (it may still run for the wanted column), and `gd`/`gD` become a
    /// whole-word first-match jump. `None` when nothing more runs.
    pub(super) fn resolve_search(&mut self, cmd: Command) -> Option<Command> {
        let Session {
            ed,
            engine,
            search_hl,
            ..
        } = self;
        // `*`/`#` (word under cursor): the planner resolves the keyword and searches; record the same
        // pattern as the last search (direction too, so `#` then `n` continues backward) and for
        // hlsearch. No keyword on the line → E348 and nothing runs.
        if !note_word_search(&cmd, &ed.ws, engine, search_hl, &mut ed.status) {
            return None;
        }
        // A search (`/pat`, `?pat`, `n`, `N`, bare, under an operator or `!`) whose pattern matches
        // nowhere fails with E486 and nothing runs (#901). The core fails it too (a beep, nothing moved or
        // edited); it still runs so the wanted column resets, as nvim's `normal_search` sets
        // `w_set_curswant` before it searches (#1171).
        // `n`/`N`/`/<CR>` before any search fail with E35 (#1173). The core fails the empty pattern (a beep,
        // nothing moved); it runs, recorded, so the wanted column resets as nvim's `normal_search` resets it.
        if no_previous_pattern(&cmd) {
            ed.run(cmd);
            ed.status.error(E35);
            return None;
        }
        if search_misses(&cmd, &mut ed.ws, &mut ed.status) {
            // A `*`/`#` that fails under 'nowrapscan' (#1558) still runs: the cursor moves to the keyword's
            // start, as nvim's `nv_ident` puts it there before it searches.
            if matches!(
                cmd,
                Command::Search { .. } | Command::SearchWordUnder { .. }
            ) {
                let err = ed.status.take();
                ed.run(cmd);
                ed.status.restore(err);
            }
            return None;
        }
        // `gd`/`gD` (declaration under cursor, TEXT heuristic — not LSP): like `*`/`#`, the engine
        // has no buffer, so resolve the keyword here and rewrite to a concrete whole-file first-match
        // jump. `gd` and `gD` are currently identical (matching nvim v0.12.4). Set the last-search to
        // the whole-word pattern forward so a following `n` continues from the landing spot (nvim's
        // `gd` sets `@/` too). `global` is unused for now — the `gd` block-scope refinement reads it.
        let cmd = if let Command::GotoDeclaration { .. } = cmd {
            match ed.ws.word_under_cursor() {
                Some(word) => {
                    let pat = format!("\\<{word}\\>");
                    engine.set_last_search(pat.clone(), true);
                    Command::GotoFirstMatch(pat)
                }
                None => {
                    ed.status.error("E348: No string under cursor");
                    return None;
                }
            }
        } else {
            cmd
        };
        Some(cmd)
    }

    /// Run a resolved command: over closed folds, through a 'formatprg'/'equalprg' filter or the tree-aware
    /// `=`, with hlsearch and the special registers refreshed, then the 'indentkeys' fix and 'foldopen'.
    pub(super) fn dispatch_resolved(&mut self, cmd: Command, frame: &FrameCtx) {
        let report = self.ui_opts.report;
        let Session {
            ed,
            highlighters,
            line_idx,
            engine,
            search_hl,
            last_ex_line,
            alt_file,
            folds,
            can_cindent,
            key_typed,
            normal_depth,
            ui_opts,
            ..
        } = self;
        let snapshot = &frame.snapshot;
        // Tree-aware `=` (F-015): if the focused buffer has a live syntax tree, resolve the reindent
        // Over closed folds (#819): `j`/`k` count a fold as one line, an operator takes it whole, `o`/`p`
        // open and put past it. Before the tree-aware rewrite, which reads the cursor line `o` opens from.
        let fv = crate::folds::for_buffer(folds, ed.ws.focused_buffer());
        let Some(cmd) = crate::app::fold_cmds::prepare(
            cmd,
            &mut ed.ws,
            fv,
            line_idx,
            snapshot,
            &mut ed.recorded,
        ) else {
            return;
        };
        // 'foldopen' applies to typed keys only (nvim checks `KeyTyped`): not to a macro, `:normal` or a
        // `:g` of them. A shift opens the fold whatever 'foldopen' holds (`op_shift`).
        let typed = *key_typed && *normal_depth == 0;
        let opens_fold = crate::app::fold_cmds::shift_opens_fold(&cmd)
            || (typed && crate::app::fold_cmds::opens_fold(&cmd, &ui_opts.foldopen));
        // `gq` under 'formatprg' / `=` under 'equalprg' filter through the program instead (#617, #618).
        if crate::app::dispatch::run_prg_operator(
            &cmd,
            &mut ed.ws,
            &ed.files,
            &mut ed.recorded,
            &mut ed.status,
        ) {
            return;
        }
        let indent_after = crate::app::indent_keys::before(&cmd, &ed.ws, highlighters, can_cindent);
        let cmd = resolve_tree_aware(cmd, &ed.ws, highlighters, line_idx, snapshot);
        // A completed search turns on hlsearch for that pattern (F-009 #1).
        if let Some(p) = search_pattern(&cmd) {
            *search_hl = Some(p);
        }
        // Refresh the read-only special registers `"/ ": ". "%` before dispatch so a `"/p` /
        // `C-r :` / `".p` / `"%p` in this command reads the live values (`:help quote_/`).
        sync_special_registers(
            &mut ed.ws,
            &mut ed.recorded,
            engine,
            last_ex_line,
            &ed.files,
            alt_file,
        );
        ed.run_reported(cmd, report);
        // Python re-indents the line its 'indentkeys' and opens leave (#1205, #1206).
        if let Some(fix) = crate::app::indent_keys::after(indent_after, &ed.ws, highlighters) {
            ed.run(fix);
        }
        // 'foldopen': a search, a horizontal move, a mark, undo… open the fold the cursor lands in.
        if opens_fold {
            if let Some(fv) = folds.get_mut(&ed.ws.focused_buffer()) {
                let pane = ed.ws.focused();
                crate::folds::open_at(
                    fv,
                    ruse_core::pos::line_of(pane.doc.bytes(), pane.view.cursor()),
                );
            }
        }
    }
}

/// Rewrite a command into its tree-aware variant when the focused buffer has a live syntax tree (F-015).
/// Three independent rewrites, each a no-op without a tree so plain-text editing is untouched:
/// - `Reindent` (`=`) → `SetIndents` with tree-computed levels (else the core bracket-depth fallback).
/// - `OpenBelow`/`OpenAbove`/`InsertNewline` (`o`/`O`/`<CR>`) → `OpenLineIndent` seeded with the suggested
///   indent for the newly opened line.
/// - `InsertChar` of a closer `}`/`)`/`]` → `InsertCloser`, which auto-dedents to the matching opener.
///
/// Pure `Command -> Command` over the workspace + highlighter cache (lifted out of the `run` loop).
pub(super) fn resolve_tree_aware(
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

/// `{count}&`: repeat the last `:s` on the cursor's line and the `count - 1` after it, KEEPING its flags.
/// nvim's default mapping is `nnoremap & :&&<CR>` (`:help &-default`), so `{count}&` is
/// `:.,.+{count-1}&&` and `j&` after `:s/a/b/g` replaces every match. A range past the last line is E16,
/// a line without a match E486, and nothing to repeat E33, as there. This is the only `&` path: the typed
/// key and the `RepeatSubstituteLine` command both come here.
pub(super) fn repeat_substitute_lines(
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

/// For `*`/`#`/`g*`/`g#` ([`Command::SearchWordUnder`]): record the keyword pattern the planner will search
/// for as the engine's last search (with its direction) and as the hlsearch pattern. `false` (with E348 on
/// the status line) when the line has no keyword, so the caller runs nothing. Any other command passes.
pub(super) fn note_word_search(
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

/// Whether `cmd` is a search the engine issued with no previous pattern to repeat (an empty one, #1173),
/// or a `gn`/`gN` with none to match (#1184).
pub(super) fn no_previous_pattern(cmd: &Command) -> bool {
    matches!(
        cmd,
        Command::Search { pattern, .. } | Command::SearchObject { pattern, .. } if pattern.is_empty()
    )
}

/// Whether `cmd` is a search whose pattern matches nowhere in the focused buffer; if so the status gets
/// Vim's `E486` (`:help E486`). The buffer-wide test is the `:g` marking rule, so it agrees with `:g` on
/// what matches; a pattern that can span a line break (`\n`, `\_x`) is left to the search itself.
/// Under 'nowrapscan' a search fails whenever it finds no target before the edge of the buffer, `*`/`#`
/// too, with `E385` (forward) or `E384` (backward) in place of `E486`, as nvim (#1558).
pub(super) fn search_misses(cmd: &Command, ws: &mut Workspace, status: &mut Status) -> bool {
    if !ws.focused().view.wrapscan() {
        return search_hits_edge(cmd, ws, status);
    }
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

/// [`search_misses`] under 'nowrapscan' (#1558): a search (`/`, `?`, `n`, `N`, `*`, `#`, `g*`, `g#`, under an
/// operator or `!` too) with no target before the edge fails with `E385` / `E384`, also when the pattern
/// matches nowhere. The planner's own span is the test, so the message agrees with what the search does.
fn search_hits_edge(cmd: &Command, ws: &mut Workspace, status: &mut Status) -> bool {
    let (pattern, backward) = match cmd {
        Command::Search {
            pattern, backward, ..
        } => (pattern.clone(), *backward),
        Command::SearchNext(pattern) => (pattern.clone(), false),
        Command::SearchPrev(pattern) => (pattern.clone(), true),
        Command::SearchWordUnder {
            forward,
            whole_word,
            ..
        } => match ws.word_search_pattern(*whole_word) {
            Some(pattern) => (pattern, !*forward),
            None => return false,
        },
        _ => return false,
    };
    if pattern.is_empty() {
        return false;
    }
    // A malformed pattern fails with the pattern reader's error first (#1087).
    if let Err(e @ ruse_core::RegexError::Syntax(_)) =
        ws.global_marks(ruse_core::SubRange::WholeFile, &pattern, false)
    {
        status.error(regex_error_msg(&e));
        return true;
    }
    if ws.span_lines(cmd).is_some() {
        return false;
    }
    status.error(crate::app::ex_range::search_hit_edge(&pattern, backward));
    true
}

/// `final_newline` is whether a write of the focused buffer ends it with a newline (`g CTRL-G` counts the
/// bytes a write gives, RFC-0024 §3).
pub(super) fn info_status(cmd: &Command, ws: &Workspace, final_newline: bool) -> Option<Msg> {
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
                final_newline,
            )))
        }
        _ => None,
    }
}

/// Resolve a Normal-mode INFO command (`ga`/`:ascii` → [`Command::AsciiInfo`], `CTRL-G` →
/// [`Command::FileInfo`], `g CTRL-G` → [`Command::CursorInfo`]) into its status-line message against the
/// focused buffer, or `None` for any other command. These print only — the buffer is never mutated — so the
/// run loop just sets `status` and skips dispatch. Splitting this out keeps the whole
/// keystroke→Command→message path unit-testable without a session.
/// `ga` is not kept in `:messages` and `CTRL-G` is (nvim v0.12.6); `g8` and `g CTRL-G` follow them.
/// Whether a write of the focused buffer ends it with a newline: `'endofline'`, or the write policy.
pub(super) fn writes_final_newline(files: &Files, ws: &Workspace) -> bool {
    files
        .write
        .add_final_newline(crate::app::file_cmds::buffer_format(
            files,
            ws.focused_buffer(),
        ))
}
