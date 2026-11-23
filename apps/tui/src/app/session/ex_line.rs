//! The Ex line pipeline: a `:`-line split at `|`, its range resolved and arguments expanded, then each
//! command routed to its family. Split from `session.rs` (#1423).

use super::{
    buffer_list_line, dest_in_range, focused_buffer_label, give_report, goto_line, lines_outcome,
    may_quit, parse_ex, place_cursor, pos_picker, quit_window, regex_error_msg, report, run_ex,
    runs_external, to_i64, BufTarget, Command, DocumentId, EditSpec, Editing, Ex, ExCmd, ExOutcome,
    ExRange, FrameCtx, KeyCode, KeyEvent, KeyModifiers, Mode, Overlay, ReadSource, Session,
    Silence, SplitDir, SwapPrompt, WriteSpec, WriteTarget,
};

impl Session {
    /// Run a finished `:`-line (F-026): split it at its `|` separators (`:help :bar`) and run each command
    /// in turn. As in Vim, a command that fails (an `E…:` status, `E492` if unknown) drops the rest of the
    /// line, and so do a quit and an interactive `:s///c` (which owns the keys until it ends).
    pub(super) fn execute_ex(&mut self, text: String, frame: &FrameCtx) {
        // An empty line (`:<CR>`, `:<C-u><CR>`) does nothing, as in Vim; it is no unknown command.
        if text.trim_matches([' ', '\t', ':']).is_empty() {
            return;
        }
        // Record the executed command-line for Vim's `":` register (`:help quote_:`), without the
        // leading `:` (the engine already strips it). Read by `C-r :` / `":p` / `@:`.
        self.last_ex_line = Some(text.clone());
        self.run_ex_pieces(crate::input::split_bar(&text), frame);
        if let Some(start) = self.pending_insert.take() {
            self.start_or_stop_insert(start);
        }
    }

    /// Run a `:startinsert`-family command (or `:stopinsert` for `None`) once its line has run (#609), the
    /// way Vim's `restart_edit` starts Insert after the command line.
    pub(super) fn start_or_stop_insert(&mut self, start: Option<(crate::input::StartKind, bool)>) {
        use crate::input::StartKind;
        let Session { ed, .. } = self;
        let mode = ed.ws.focused().view.mode();
        let Some((kind, bang)) = start else {
            if mode != Mode::Normal {
                // Only `i_CTRL-O` runs an Ex command in Insert mode; its return is dropped, and the
                // cursor stays where `CTRL-O` left it (no `<Esc>` step back) unless it sat past the end.
                let at = ed.ws.focused().view.cursor().0;
                ed.run(Command::EnterNormal);
                let b = ed.ws.focused().doc.bytes();
                if at < ruse_core::pos::line_end(b, ruse_core::BytePos(at)).0 {
                    place_cursor(&mut ed.ws, &mut ed.recorded, at);
                }
            }
            return;
        };
        if mode != Mode::Normal {
            return;
        }
        let enter = match kind {
            StartKind::Insert if bang => Command::AppendLineEnd,
            StartKind::Insert => Command::EnterInsert,
            StartKind::Replace => Command::EnterReplace,
            StartKind::VirtualReplace => Command::EnterVirtualReplace,
        };
        if bang && kind != StartKind::Insert {
            // `:startreplace!` replaces from the end of the line: enter at the line's end, as `A` does.
            ed.run(Command::AppendLineEnd);
            ed.run(Command::EnterNormal);
            let pane = ed.ws.focused();
            let b = pane.doc.bytes();
            let end = ruse_core::pos::line_end(b, pane.view.cursor()).0;
            place_cursor(&mut ed.ws, &mut ed.recorded, end);
        }
        ed.run(enter);
    }

    /// Run the `|`-separated commands of one line in turn (see [`Session::execute_ex`]). A backwards range
    /// stops the line at its prompt; the commands after it wait in the prompt for its answer.
    pub(super) fn run_ex_pieces(&mut self, pieces: Vec<String>, frame: &FrameCtx) {
        let mut pieces = pieces.into_iter();
        while let Some(cmd) = pieces.next() {
            self.ed.status.clear();
            self.ex_line = cmd.clone();
            // Command modifiers (`:silent`, `:verbose`, …) come first (#609). `:silent` hides the command's
            // message but not an error; `:silent!` hides the error too, and the line goes on after it. The
            // hidden error is still the last error (Vim's `v:errmsg`).
            let (mods, cmd) = crate::input::strip_modifiers(&cmd);
            if mods.filter.is_some() && cmd.trim().is_empty() {
                self.ed.status.error("E476: Invalid command");
                break;
            }
            // `:keeppatterns`, `:keepalt` and `:keepjumps` (#609): put back what the command changed.
            let kept_patterns = mods.keep_patterns.then(|| {
                (
                    self.engine.search_state(),
                    self.search_hl.clone(),
                    self.last_substitute.clone(),
                )
            });
            let kept_alt = mods
                .keep_alt
                .then(|| (self.ed.ws.alternate(), self.alt_file.clone()));
            // The switch goes through the trace (RFC-0020 R6): the command it wraps replays without the
            // jumps it skipped live.
            let jumps_were_kept = self.ed.ws.keep_jumps();
            if mods.keep_jumps && !jumps_were_kept {
                run_ex(
                    ExCmd::KeepJumps(true),
                    &mut self.ed.ws,
                    &mut self.ed.recorded,
                );
            }
            self.ex_filter = mods.filter.clone();
            self.ex_verbose = mods.verbose;
            self.ex_confirm = mods.confirm;
            // What `:silent` keeps off the screen stays out of `:messages` too (#1235).
            let hushed = self.ed.status.hush(if mods.silent_errors {
                Silence::All
            } else if mods.silent {
                Silence::Messages
            } else {
                Silence::Off
            });
            self.execute_ex_one(cmd.to_string(), frame);
            self.ed.status.unhush(hushed);
            self.ex_filter = None;
            self.ex_verbose = false;
            if !jumps_were_kept && self.ed.ws.keep_jumps() {
                run_ex(
                    ExCmd::KeepJumps(false),
                    &mut self.ed.ws,
                    &mut self.ed.recorded,
                );
            }
            if let Some((search, hl, sub)) = kept_patterns {
                self.engine.restore_search_state(search);
                self.search_hl = hl;
                self.last_substitute = sub;
            }
            if let Some((alt, alt_file)) = kept_alt {
                self.ed.ws.set_alternate(alt);
                self.alt_file = alt_file;
            }
            self.ex_confirm = false;
            if let Some(prompt) = &mut self.swap {
                prompt.pending.extend(pieces);
                return;
            }
            if let Some(prompt) = &mut self.changed_prompt {
                prompt.pending.extend(pieces);
                return;
            }
            if mods.silent_errors && self.ed.status.is_error() {
                self.silenced_error = Some(self.ed.status.take().text);
            } else if mods.silent && !self.ed.status.is_error() {
                self.ed.status.clear();
            }
            if self.ed.quit || self.confirm.is_some() || self.ed.status.is_error() {
                break;
            }
        }
    }

    /// The key that answered a backwards-range prompt (`ask_yesno` in nvim): `y` swaps the range and runs
    /// the rest of the line, `n`, `<Esc>` or `CTRL-C` drop the whole line, and any other key is ignored
    /// while the prompt stays up.
    pub(super) fn answer_swap(&mut self, prompt: SwapPrompt, key: KeyEvent, frame: &FrameCtx) {
        let plain = !key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
        match key.code {
            KeyCode::Char('y') if plain => {
                self.ed.status.clear();
                self.run_ex_pieces(prompt.pending, frame);
            }
            KeyCode::Char('n') | KeyCode::Esc if plain => self.ed.status.clear(),
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.ed.status.clear()
            }
            _ => self.swap = Some(prompt),
        }
    }

    /// The key that answered a `'confirm'` dialog (#619, [`crate::app::abandon`]): act on it and run the
    /// command again, or keep the prompt up for a key that is no answer.
    pub(super) fn answer_changed(
        &mut self,
        prompt: crate::app::abandon::ChangedPrompt,
        key: KeyEvent,
        frame: &FrameCtx,
    ) {
        let Some(a) = crate::app::abandon::answer(&prompt, key) else {
            self.changed_prompt = Some(prompt);
            return;
        };
        self.ed.status.clear();
        if crate::app::abandon::act(
            &prompt,
            a,
            &mut self.ed.ws,
            &self.ed.files,
            &mut self.ed.status,
        ) && !self.ed.status.is_error()
        {
            self.ed.status.clear();
            self.run_ex_pieces(prompt.pending, frame);
        }
    }

    /// Run ONE Ex command (a `|`-free piece of the typed line). This is the single Ex dispatch (#892):
    /// every `Ex` variant has exactly one arm here, so a new command cannot land in a match that never sees
    /// it.
    pub(super) fn execute_ex_one(&mut self, text: String, frame: &FrameCtx) {
        let parsed = parse_ex(&text);
        // Unsaved changes in a buffer the command leaves (#619): 'hidden', 'autowrite', 'confirm'.
        let line = self.ex_line.clone();
        match crate::app::abandon::gate(
            &parsed,
            &mut self.ed.ws,
            &self.ed.files,
            &self.ui_opts.buffers,
            self.ex_confirm,
            &line,
            &mut self.ed.status,
        ) {
            crate::app::abandon::Gate::Go => {}
            crate::app::abandon::Gate::Stop => return,
            crate::app::abandon::Gate::Ask(prompt) => {
                self.ed
                    .status
                    .echo(crate::app::abandon::prompt_text(&prompt, &self.ed.ws));
                self.changed_prompt = Some(prompt);
                return;
            }
        }
        let external = runs_external(&parsed);
        if external {
            self.sync_journals();
        }
        self.execute_ex_parsed(text, frame);
        // An edit `'nomodifiable'` refused (#619): nvim's E21 over whatever the command said.
        if self.ed.ws.take_not_modifiable() {
            self.ed.status.error(ruse_core::E21);
        }
        // The files may have changed: after another program ran (nvim checks after a shell command), and
        // after a write, which may have been refused because one did (RFC-0023 §8). `:checktime` asks.
        self.check_disk(external || parsed == Ex::CheckTime);
    }

    /// The `:` line's leading range resolved (`app::ex_range`), or `None` when that is all it does: `:[count]messages`
    /// (whose count is no line number), a range with no command (`:5` goes to the line), a backwards range
    /// (asked before it is swapped) and an error. A `/pat/` address becomes the last search, and a `;` that
    /// moved the cursor leaves it moved. The text that remains is the command.
    pub(super) fn resolve_ex_line(&mut self, text: &str) -> Option<String> {
        let Session {
            ed,
            engine,
            folds,
            swap,
            search_hl,
            last_substitute,
            global_busy,
            ..
        } = self;
        // The leading range (`.`, `$`, offsets, marks, `/pat/`, `'<,'>` from Visual `:`) resolves to line
        // numbers (`app::ex_range`); one past the buffer is E16 (#1079), a range with no command goes to its
        // line (`:5`, `:$`), and a backwards one asks before it is swapped.
        // `:[count]messages [clear]` (#1235): its count is no line number, so it is read before the range.
        if let Some(cmd) = crate::app::messages::parse_messages(text) {
            match cmd {
                Ok(cmd) if cmd.clear => ed.status.history_mut().keep_last(cmd.count.unwrap_or(0)),
                Ok(cmd) => {
                    let shown: Vec<&str> = ed.status.history().last(cmd.count).collect();
                    let shown = shown.join("\n");
                    ed.status.echo(shown);
                }
                Err(e) => ed.status.error(e.to_string()),
            }
            return None;
        }
        let pats = crate::app::ex_range::AddrPatterns {
            search: engine.last_search(),
            substitute: last_substitute.as_ref().map(|(p, _, _)| p.as_str()),
        };
        // Inside a `:g` a range does not grow to closed folds (nvim checks `global_busy`).
        let range_folds = (!*global_busy)
            .then(|| folds.get(&ed.ws.focused_buffer()).map(Vec::as_slice))
            .flatten();
        let resolved = crate::app::ex_range::resolve_ex_range(text, &mut ed.ws, pats, range_folds);
        // A `/pat/` or `?pat?` address becomes the last search, as in Vim (`:/foo/d` then `n`).
        if let Some((pat, forward)) = resolved.search {
            engine.set_last_search(pat.clone(), forward);
            *search_hl = Some(pat);
        }
        // A `;` in the range moved the cursor, and it stays moved whatever the command does (`:5;-2d`).
        if let Some(line) = resolved.cursor {
            goto_line(line, ed);
        }
        match resolved.range {
            Ok(ExRange::Line(text)) => Some(text),
            Ok(ExRange::Backwards(swapped)) => {
                ed.status.echo("Backwards range given, OK to swap (y/n)?");
                *swap = Some(SwapPrompt {
                    pending: vec![swapped],
                });
                None
            }
            Ok(ExRange::Goto(line)) => {
                goto_line(line, ed);
                None
            }
            Err(e) => {
                ed.status.error(e.to_string());
                None
            }
        }
    }

    /// `ex` with its file names and shell commands expanded: `%`/`#`/`#n` in a file argument, and `!` (the
    /// previous command, remembered for the next) in a shell command.
    pub(super) fn expand_ex_args(&mut self, ex: Ex) -> Result<Ex, crate::input::ExpandError> {
        let Session {
            ed: Editing { ws, files, .. },
            alt_file,
            last_shell,
            ..
        } = self;
        // The current, alternate and numbered file names, for `%` / `#` / `#n` in a file argument or shell
        // command.
        let file_name = |id: Option<DocumentId>| {
            id.and_then(|id| files.get(&id))
                .map(|bf| bf.path.to_string_lossy().into_owned())
        };
        let (cur_name, alt_name) = (
            file_name(Some(ws.focused_buffer())),
            crate::app::ex_more::alternate_name(alt_file, ws, files),
        );
        let numbered: Vec<(u64, String)> = files
            .iter()
            .map(|(id, bf)| (id.0, bf.path.to_string_lossy().into_owned()))
            .collect();
        let names = crate::input::FileNames {
            current: cur_name.as_deref(),
            alternate: alt_name.as_deref(),
            buffers: &numbered,
        };
        let expand = |arg: &str| crate::input::expand_filename(arg, &names);
        let expand_opt = |arg: Option<String>| arg.map(|a| expand(&a)).transpose();
        // A shell command: `%`/`#` first, then `!` → the previous command; remembered for the next `!`.
        let mut shell_cmd = |cmd: &str| -> Result<String, crate::input::ExpandError> {
            let cmd = crate::input::expand_bang(&expand(cmd)?, last_shell.as_deref())?;
            *last_shell = Some(cmd.clone());
            Ok(cmd)
        };
        match ex {
            Ex::Edit(spec) => {
                expand_opt(spec.file.clone()).map(|file| Ex::Edit(EditSpec { file, ..spec }))
            }
            Ex::Find { split, spec } => expand_opt(spec.file.clone()).map(|file| Ex::Find {
                split,
                spec: EditSpec { file, ..spec },
            }),
            Ex::Drop(spec) => {
                expand_opt(spec.file.clone()).map(|file| Ex::Drop(EditSpec { file, ..spec }))
            }
            Ex::SaveAs { bang, file, opts } => {
                expand(&file).map(|file| Ex::SaveAs { bang, file, opts })
            }
            Ex::Write(spec) => match &spec.target {
                WriteTarget::File(f) => expand_opt(f.clone()).map(WriteTarget::File),
                WriteTarget::Append(f) => expand_opt(f.clone()).map(WriteTarget::Append),
                WriteTarget::Command(c) => shell_cmd(c).map(WriteTarget::Command),
            }
            .map(|target| Ex::Write(WriteSpec { target, ..spec })),
            Ex::Read {
                addr,
                source: ReadSource::File(path),
            } => expand(&path).map(|p| Ex::Read {
                addr,
                source: ReadSource::File(p),
            }),
            Ex::Read {
                addr,
                source: ReadSource::Command(cmd),
            } => shell_cmd(&cmd).map(|c| Ex::Read {
                addr,
                source: ReadSource::Command(c),
            }),
            Ex::Filter { range, cmd } => shell_cmd(&cmd).map(|cmd| Ex::Filter { range, cmd }),
            Ex::Shell(cmd) => shell_cmd(&cmd).map(Ex::Shell),
            // `:cd -` is the previous directory, not a file name.
            Ex::Cd(Some(dir)) if dir != "-" => expand(&dir).map(|dir| Ex::Cd(Some(dir))),
            Ex::File(crate::input::FileCmd::Rename(name)) => {
                expand(&name).map(|name| Ex::File(crate::input::FileCmd::Rename(name)))
            }
            ex => Ok(ex),
        }
    }

    pub(super) fn execute_ex_parsed(&mut self, text: String, frame: &FrameCtx) {
        let Some(text) = self.resolve_ex_line(&text) else {
            return;
        };
        let ex = match self.expand_ex_args(parse_ex(&text)) {
            Ok(ex) => ex,
            Err(e) => {
                self.ed.status.error(e.to_string());
                return;
            }
        };
        // Every command is routed by name, so a new `Ex` variant fails the build here.
        let after = match ex {
            ex @ (Ex::NoHighlight
            | Ex::InspectConfig(..)
            | Ex::Diagnostics
            | Ex::Registers
            | Ex::Ascii
            | Ex::Digraphs
            | Ex::Marks
            | Ex::Jumps
            | Ex::Changes
            | Ex::CheckHealth
            | Ex::Buffers
            | Ex::Pwd
            | Ex::Print { .. }
            | Ex::Z(..)
            | Ex::LineNumber(..)) => self.ex_show(ex, frame),
            ex @ (Ex::Format
            | Ex::Rename(..)
            | Ex::References
            | Ex::CodeAction
            | Ex::Trust
            | Ex::Untrust
            | Ex::ConfigReload) => self.ex_services(ex, frame),
            ex @ (Ex::WriteAll { .. }
            | Ex::Quit
            | Ex::QuitForce
            | Ex::QuitAll { .. }
            | Ex::CQuit(..)
            | Ex::Hide
            | Ex::Close
            | Ex::Split
            | Ex::VSplit
            | Ex::Only
            | Ex::Enew
            | Ex::BufferNext
            | Ex::BufferPrev
            | Ex::Buffer(..)
            | Ex::BufferDelete { .. }) => self.ex_windows(ex),
            ex @ (Ex::Edit(..)
            | Ex::Read { .. }
            | Ex::Write(..)
            | Ex::SaveAs { .. }
            | Ex::Cd(..)
            | Ex::File(..)
            | Ex::Find { .. }
            | Ex::Drop(..)
            | Ex::SaveTrace(..)
            | Ex::Terminal) => self.ex_files(ex, frame),
            ex @ (Ex::Delete(..)
            | Ex::Yank(..)
            | Ex::Join { .. }
            | Ex::Move(..)
            | Ex::Copy(..)
            | Ex::Shift { .. }
            | Ex::Put { .. }
            | Ex::Sort(..)
            | Ex::Retab { .. }
            | Ex::Substitute(..)
            | Ex::RepeatSubstitute { .. }
            | Ex::Global(..)
            | Ex::Normal { .. }
            | Ex::Filter { .. }
            | Ex::Shell(..)
            | Ex::Earlier(..)
            | Ex::Later(..)
            | Ex::Mark { .. }
            | Ex::Fold(..)
            | Ex::FoldOpen { .. }
            | Ex::FoldDo { .. }) => self.ex_lines(ex, frame),
            ex @ (Ex::SetOptions(..)
            | Ex::Lmap { .. }
            | Ex::Lunmap { .. }
            | Ex::StartInsert { .. }
            | Ex::StopInsert
            | Ex::CheckTime
            | Ex::Fail(..)
            | Ex::Unknown(..)) => self.ex_misc(ex, frame),
        };
        if let Some(grant) = after.trust_change {
            self.change_config_trust(grant);
        }
        if after.config_reload {
            self.reload_config(true);
        }
        if let Some(line) = after.then_run {
            let opened = self.ed.status.take();
            self.execute_ex_one(line, frame);
            // The open's own report stays unless the command said something (an error, a count).
            if self.ed.status.is_empty() {
                self.ed.status.restore(opened);
            }
        }
    }

    /// The Ex commands that only show something: messages, listings and the view-only overlays.
    pub(super) fn ex_show(&mut self, ex: Ex, frame: &FrameCtx) -> ExAfter {
        let after = ExAfter::default();
        let Session {
            ed: Editing { ws, status, .. },
            search_hl,
            overlay,
            ..
        } = self;
        match ex {
            Ex::NoHighlight => *search_hl = None, // `:noh` clears the search highlight (F-009 #1)
            Ex::InspectConfig(key) => self.ex_inspect_config(key),
            Ex::Diagnostics => self.ex_diagnostics(frame),
            Ex::Registers => self.ex_registers(),
            // `:ascii`/`:as` — the ex synonym of `ga`: print the char-under-cursor's value.
            Ex::Ascii => {
                let pane = ws.focused();
                status.echo(ruse_core::ascii_info(pane.doc.bytes(), pane.view.cursor()));
            }
            // `:digraphs` / `:dig`: open the view-only digraph-listing overlay.
            Ex::Digraphs => {
                let p = crate::ui::digraph_picker::open();
                status.echo(format!("{} digraph(s)", p.rows().len()));
                *overlay = Some(Overlay::Digraphs(p));
            }
            Ex::Marks => self.ex_marks(),
            // `:jumps` / `:changes` (F-003): position viewers over the jumplist / change list;
            // Enter jumps. Both use the position-viewer overlay.
            ex @ (Ex::Jumps | Ex::Changes) => {
                let positions = if matches!(ex, Ex::Jumps) {
                    ws.jumps_snapshot()
                } else {
                    ws.changes_snapshot()
                };
                if positions.is_empty() {
                    status.echo("empty");
                } else {
                    status.echo(format!("{} position(s)", positions.len()));
                    *overlay = Some(Overlay::Positions(pos_picker::open(
                        positions.into_iter().map(|p| p.0).collect(),
                        ws.focused().doc.bytes(),
                    )));
                }
            }
            Ex::CheckHealth => self.ex_checkhealth(),
            Ex::Buffers => status.echo(buffer_list_line(ws)),
            Ex::Print { range, flags } => self.ex_print(range, flags),
            Ex::Z(spec) => self.ex_z(spec, frame),
            Ex::LineNumber(range) => self.ex_line_number(range),
            // `:pwd` / `:cd` (#610). Buffer files keep pointing at the same files across a `:cd`.
            Ex::Pwd => match std::env::current_dir() {
                Ok(d) => status.info(d.display().to_string()),
                Err(e) => status.error(e.to_string()),
            },
            _ => unreachable!("routed by execute_ex_parsed"),
        }
        after
    }

    /// The Ex commands for the language servers, workspace trust and the config layers.
    pub(super) fn ex_services(&mut self, ex: Ex, frame: &FrameCtx) -> ExAfter {
        let mut after = ExAfter::default();
        let Session {
            ed: Editing {
                ws, files, status, ..
            },
            lsp,
            ..
        } = self;
        let snapshot = &frame.snapshot;
        match ex {
            // `:fmt` / `:rename {new}` / `:references` / `:codeaction` (F-014): the coordinator sends
            // the request; the response is dispatched + applied (or opens a picker) on a later frame.
            ex @ (Ex::Format | Ex::Rename(_) | Ex::References | Ex::CodeAction) => {
                lsp.on_ex(&ex, ws, files, snapshot, status);
            }
            // `:trust` (D-058): the user grants workspace trust for this session; language servers
            // start on the next frame's sync. It also records the content-bound trust of the workspace
            // config (D-062, #614), and `:untrust` removes that record (both below, after this match).
            Ex::Trust => {
                lsp.grant_trust(status);
                after.trust_change = Some(true);
            }
            Ex::Untrust => after.trust_change = Some(false),
            // `:config-reload` (Helix's name, #1152): read the config layers again now (below, after this
            // match); it also leaves safe mode.
            Ex::ConfigReload => after.config_reload = true,
            _ => unreachable!("routed by execute_ex_parsed"),
        }
        after
    }

    /// The window, buffer and quit commands.
    pub(super) fn ex_windows(&mut self, ex: Ex) -> ExAfter {
        let after = ExAfter::default();
        let Session {
            ed:
                Editing {
                    ws,
                    files,
                    quit,
                    status,
                    ..
                },
            highlighters,
            alt_file,
            #[cfg(unix)]
            terminals,
            ..
        } = self;
        match ex {
            Ex::BufferDelete { force } => self.ex_bdelete(force),
            // `:wa` / `:wqa` / `:xa`: write every modified buffer with a file (a terminal has none to write).
            Ex::WriteAll { quit: then_quit } => {
                #[cfg(unix)]
                let skip = |id: DocumentId| terminals.contains_key(&id);
                #[cfg(not(unix))]
                let skip = |_: DocumentId| false;
                let ok = crate::app::file_cmds::ex_write_all(ws, files, status, skip);
                if ok && then_quit {
                    *quit = may_quit(ws, status, false);
                }
            }
            Ex::Quit => quit_window(ws, status, quit, false),
            Ex::QuitForce => quit_window(ws, status, quit, true),
            Ex::QuitAll { force: true } => *quit = true,
            Ex::QuitAll { force: false } => *quit = may_quit(ws, status, false),
            Ex::CQuit(code) => {
                crate::app::dispatch::set_exit_status(code);
                *quit = true;
            }
            Ex::Hide | Ex::Close => {
                if !ws.close_focused() {
                    status.error("E444: Cannot close last window");
                }
            }
            // F-007 window commands: split the focused window onto the same buffer, or close it.
            Ex::Split => {
                ws.split(SplitDir::Horizontal);
                status.echo("window split");
            }
            Ex::VSplit => {
                ws.split(SplitDir::Vertical);
                status.echo("window vsplit");
            }
            Ex::Only => self.ex_only(),
            // F-007 multi-buffer navigation. `:enew` opens a scratch buffer and focuses it; `:ls` lists the
            // buffers; `:bn`/`:bp` cycle; `:b {n}`/`:b#` switch by number / to the alternate.
            Ex::Enew => {
                let id = ws.add_buffer(Vec::new(), None);
                ws.focus_buffer(id);
                status.echo(format!("[No Name] (buffer {})", id.0));
            }
            Ex::BufferNext => {
                ws.cycle_buffer(true);
                status.echo(focused_buffer_label(ws));
            }
            Ex::BufferPrev => {
                ws.cycle_buffer(false);
                status.echo(focused_buffer_label(ws));
            }
            Ex::Buffer(BufTarget::Alternate)
                if crate::app::ex_more::AltFile::current(alt_file, ws).is_some() =>
            {
                // The alternate file a write or rename named has no buffer yet: edit it (nvim's `:b#`
                // goes to the buffer it made for the name).
                let name = crate::app::ex_more::AltFile::current(alt_file, ws)
                    .unwrap_or_default()
                    .to_string();
                let spec = EditSpec {
                    bang: false,
                    file: Some(name),
                    cmd: None,
                    opts: crate::input::FileOpts::default(),
                };
                crate::app::file_cmds::ex_edit(&spec, ws, files, highlighters, status);
            }
            Ex::Buffer(target) => {
                let id = match target {
                    BufTarget::Number(n) => Some(DocumentId(n)),
                    BufTarget::Alternate => ws.alternate(),
                };
                match id {
                    Some(id) if ws.focus_buffer(id) => status.echo(focused_buffer_label(ws)),
                    Some(id) => status.error(format!("E86: buffer {} does not exist", id.0)),
                    None => status.error("E23: no alternate buffer"),
                }
            }
            _ => unreachable!("routed by execute_ex_parsed"),
        }
        after
    }

    /// The file commands: edit, read, write, the trace, and the terminal.
    pub(super) fn ex_files(&mut self, ex: Ex, frame: &FrameCtx) -> ExAfter {
        let mut after = ExAfter::default();
        let Session { .. } = self;
        match ex {
            Ex::Terminal => self.ex_terminal(frame),
            Ex::Edit(spec) => after.then_run = self.ex_edit(spec),
            Ex::Read { addr, source } => self.ex_read(addr, source),
            Ex::Write(spec) => self.ex_write(spec),
            Ex::SaveAs { bang, file, opts } => self.ex_saveas(bang, file, opts),
            Ex::SaveTrace(p) => self.ex_save_trace(p),
            Ex::Cd(dir) => self.ex_cd(dir),
            Ex::File(cmd) => self.ex_file(cmd),
            Ex::Find { split, spec } => after.then_run = self.ex_find(split, spec),
            Ex::Drop(spec) => after.then_run = self.ex_drop(spec),
            _ => unreachable!("routed by execute_ex_parsed"),
        }
        after
    }

    /// The line-editing Ex commands: delete/yank/put, move/copy, shifts, sort, substitute, `:g`, `:normal`, filters, folds and time travel.
    pub(super) fn ex_lines(&mut self, ex: Ex, frame: &FrameCtx) -> ExAfter {
        let after = ExAfter::default();
        let Session {
            ed,
            engine,
            ui_opts,
            ..
        } = self;
        match ex {
            Ex::Mark { range, name } => self.ex_mark(range, name),
            // `:[range]normal[!] {keys}` replays the key payload through the input `engine` → the same
            // `Command` pipeline as typed keys / macro replay.
            Ex::Normal { range, keys, .. } => self.ex_normal(range, &keys, frame),
            ex @ Ex::Global(_) => self.ex_global(ex, frame),
            Ex::Fold(range) => self.ex_fold(range),
            Ex::FoldOpen {
                range,
                open,
                recursive,
            } => self.ex_fold_open(range, open, recursive),
            Ex::FoldDo { range, closed, cmd } => self.ex_fold_do(range, closed, cmd, frame),
            Ex::Earlier(n) => time_travel(true, n, ed),
            Ex::Later(n) => time_travel(false, n, ed),
            Ex::Filter { range, cmd } => self.ex_filter(range, cmd),
            Ex::Shell(cmd) => self.ex_shell(cmd),
            Ex::RepeatSubstitute { range, flags } => self.ex_repeat_substitute(range, flags),
            ex @ Ex::Substitute(_) => self.ex_substitute(ex),
            Ex::Delete(range, reg, flags) => self.ex_delete(range, reg, flags),
            Ex::Yank(range, reg) => self.ex_yank(range, reg),
            Ex::Join { range, bang } => self.ex_join(range, bang),
            // `:[range]m {addr}` — move the range's lines to after the destination line.
            Ex::Move(_, dest) | Ex::Copy(_, dest) if !dest_in_range(&ed.ws, dest) => {
                ed.status.error("E16: Invalid range");
            }
            Ex::Move(range, dest) => {
                let ((first, last), dest) = (ed.ws.resolve_range(range), ed.ws.resolve_addr(dest));
                let moved = dest.and_then(|dest| {
                    match run_ex(
                        ExCmd::Move { first, last, dest },
                        &mut ed.ws,
                        &mut ed.recorded,
                    ) {
                        ExOutcome::Relocated(n) => n,
                        _ => None,
                    }
                });
                match moved {
                    // Moved onto itself (`:2m2`): nothing to report.
                    Some(n) => give_report(&mut ed.status, report::moved(n, ui_opts.report)),
                    None => ed
                        .status
                        .error("E134: Cannot move a range of lines into itself"),
                }
            }
            Ex::Copy(range, dest) => self.ex_copy(range, dest),
            Ex::Shift {
                range,
                left,
                levels,
            } => self.ex_shift(range, left, levels),
            Ex::Put { addr, reg, above } => self.ex_put(addr, reg, above),
            // `:[range]sort[!] [i][n][r][u] [/pattern/]` — sort the range's lines (whole file with no range).
            mut ex @ Ex::Sort(..) => {
                if let Err(msg) = crate::input::reuse_last_search(&mut ex, engine.last_search()) {
                    ed.status.error(msg.to_string());
                    return after;
                }
                let Ex::Sort(range, spec) = ex else {
                    unreachable!("bound by the arm's pattern")
                };
                let (first, last) = ed.ws.resolve_range(range);
                let ex = ExCmd::Sort {
                    first,
                    last,
                    opts: spec,
                };
                let sorted = match run_ex(ex, &mut ed.ws, &mut ed.recorded) {
                    ExOutcome::Failed(e) => Err(e),
                    out => Ok(lines_outcome(&out)),
                };
                match sorted {
                    // nvim's `msgmore` for the lines `u` dropped; a sort that drops none says nothing.
                    Ok(removed) => give_report(
                        &mut ed.status,
                        report::more(-to_i64(removed), ui_opts.report),
                    ),
                    // The pattern reader's own error, as `:sort` shows it (`E54`, no `E476` after it).
                    Err(e) => ed.status.error(regex_error_msg(&e)),
                }
            }
            Ex::Retab {
                range,
                bang,
                tabstop,
            } => self.ex_retab(range, bang, tabstop),
            _ => unreachable!("routed by execute_ex_parsed"),
        }
        after
    }

    /// Options, language maps, the Insert starts, `:checktime` and the parse errors.
    pub(super) fn ex_misc(&mut self, ex: Ex, frame: &FrameCtx) -> ExAfter {
        let after = ExAfter::default();
        let Session {
            engine,
            ed: Editing { status, .. },
            pending_insert,
            ..
        } = self;
        match ex {
            Ex::SetOptions(line) => self.ex_set(line, frame),
            // Lang-Arg map maintenance (F-027): the map is engine state.
            Ex::Lmap { lhs, rhs } => engine.set_lang_mapping(lhs, rhs),
            Ex::Lunmap { lhs } => engine.clear_lang_mapping(lhs),
            // `:earlier`/`:later` walk chronological undo time by recording + applying N of the
            // g-/g+ commands (done HERE so they land in `recorded` for trace fidelity).
            // `:checktime`: the look itself follows the command ([`Session::execute_ex_one`]).
            Ex::CheckTime => {}
            // `:startinsert` and friends (#609) start Insert once the whole line has run.
            Ex::StartInsert { kind, bang } => *pending_insert = Some(Some((kind, bang))),
            Ex::StopInsert => *pending_insert = Some(None),
            Ex::Fail(msg) => status.error(msg),
            Ex::Unknown(s) => status.error(format!("E492: Not an editor command: {s}")),
            _ => unreachable!("routed by execute_ex_parsed"),
        }
        after
    }
}

/// What an Ex command asks the session to do once its own work is done.
#[derive(Default)]
pub(super) struct ExAfter {
    /// An Ex line to run after it (`:e +cmd`).
    then_run: Option<String>,
    /// `:trust` / `:untrust`: record or remove the workspace config's trust.
    trust_change: Option<bool>,
    /// `:config-reload`: read the config layers again.
    config_reload: bool,
}

/// `:earlier {n}` (`older`) / `:later {n}`: `n` steps of `g-` / `g+`, recorded and applied one by one so
/// each lands in the trace. The walk stops at the end of the undo time line, where a step changes nothing,
/// so a huge count costs no more steps than there are changes (#685); CTRL-C stops it too.
pub(super) fn time_travel(older: bool, n: u32, ed: &mut Editing) {
    let cmd = if older {
        Command::UndoOlder
    } else {
        Command::UndoNewer
    };
    for _ in 0..n {
        let before = ed.ws.focused().doc.revision();
        ed.run(cmd.clone());
        if ed.ws.focused().doc.revision() == before || ruse_core::interrupt::breakcheck() {
            break;
        }
    }
    let dir = if older { "earlier" } else { "later" };
    ed.status.info(format!("{n} change(s) {dir}"));
}
