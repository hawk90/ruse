//! The Emacs profile's file and buffer commands that ask (#619), as Emacs 31.1 (`emacs -Q`) runs them:
//! `save-buffers-kill-terminal` (`C-x C-c`), `save-some-buffers` (`C-x s`), `revert-buffer`,
//! `revert-buffer-quick` (`C-x x g`), `read-only-mode` (`C-x C-q`, with `view-read-only`'s View mode) and
//! `find-file` (`C-x C-f`, with `query-about-changed-file` and `revert-without-query`).
//!
//! The questions are Emacs's, in the echo area:
//! - `yes-or-no-p`: the answer is typed and ended with `RET`; anything but `yes` or `no` says
//!   `Please answer yes or no.` and asks again;
//! - `y-or-n-p`: one key, `y`/`SPC` or `n`/`DEL`; another key asks again after `Please answer y or n.  `;
//! - `map-y-or-n-p` (save-some-buffers): `y`/`SPC` saves, `n`/`DEL` skips, `!` saves this and the rest,
//!   `.` saves this one and stops, `q`/`RET` stops, `M-~` skips and marks the buffer unmodified;
//! - `C-g` quits any of them (`Quit`).
//!
//! `C-x C-c` follows `save-buffers-kill-emacs`: it offers every modified buffer (`save-some-buffers` with
//! PRED `t`, so a non-file buffer with `buffer-offer-save` too), then asks `Modified buffers exist; exit
//! anyway?` if one is left, `Active processes exist; kill them and exit anyway?` for a running terminal
//! (`confirm-kill-processes`), and `Really exit Emacs?` (`confirm-kill-emacs`) unless it already asked.

use super::*;
use crate::app::write_opts::EmacsFiles;
use crate::input::EmacsAction;

/// What answering the question does next.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Then {
    /// `C-x C-c` past save-some-buffers: the checks from `stage` on (`asked`: a yes-or-no question was
    /// already answered, so `confirm-kill-emacs` is not asked).
    Kill { stage: u8, asked: bool },
    /// Re-read the focused buffer from its file.
    Revert,
    /// Re-read buffer `id` (find-file's changed-on-disk question).
    RevertBuffer(DocumentId),
    /// `write-file` to the existing file named here, now confirmed.
    WriteFile(String),
    /// Nothing more (save-some-buffers on its own).
    Done,
}

/// An Emacs question waiting for its answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum EmacsPrompt {
    /// `yes-or-no-p`: the text typed so far.
    YesOrNo {
        question: String,
        typed: String,
        then: Then,
    },
    /// `y-or-n-p`.
    YOrN { question: String, then: Then },
    /// `map-y-or-n-p` over `queue` (save-some-buffers), at `at`.
    SaveSome {
        queue: Vec<DocumentId>,
        at: usize,
        saved: usize,
        then: Then,
    },
    /// `abort-if-file-too-large`: open the large file `name`, or not, or literally (`'binary'`).
    LargeFile { name: String, question: String },
    /// A file name read in the minibuffer: `find-file`, or `write-file` for a buffer without one.
    ReadFile {
        prompt: String,
        typed: String,
        purpose: ReadFor,
    },
}

/// What a file name is read for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ReadFor {
    /// `find-file`; `confirmed`: a new file's name was entered once under `confirm-nonexistent-file-or-buffer`.
    FindFile { confirmed: bool },
    /// `write-file`.
    WriteFile,
    /// Saving buffer `buf` (no file) inside save-some-buffers, then going on with the queue.
    SaveAs {
        buf: DocumentId,
        rest: Box<EmacsPrompt>,
    },
}

/// The `save-some-buffers` question for `id`.
fn save_question(s: &Session, id: DocumentId) -> String {
    let keys = "(y, n, !, ., q, M-~, C-r, C-f, d or C-h) ";
    match s.files.get(&id) {
        Some(bf) => {
            let file = std::path::absolute(&bf.path).unwrap_or_else(|_| bf.path.clone());
            let base = file
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let name = s.ws.buffer_name(id).unwrap_or_default();
            let shown = std::path::Path::new(name)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            if shown == base {
                format!("Save file {}? {keys}", file.display())
            } else {
                format!("Save file {} (buffer {name})? {keys}", file.display())
            }
        }
        None => format!(
            "Save buffer {}? {keys}",
            s.ws.buffer_name(id).unwrap_or("*scratch*")
        ),
    }
}

fn plain(key: KeyEvent, c: char) -> bool {
    key.code == KeyCode::Char(c)
        && !key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
}

fn ctrl_g(key: KeyEvent) -> bool {
    is_ctrl(key, 'g')
}

impl Session {
    fn emacs_vars(&self) -> EmacsFiles {
        self.files.write.emacs.clone().unwrap_or_default()
    }

    /// Show `p` and wait for its answer.
    fn ask(&mut self, p: EmacsPrompt) {
        let text = match &p {
            EmacsPrompt::YesOrNo {
                question, typed, ..
            } => format!("{question}(yes or no) {typed}"),
            EmacsPrompt::YOrN { question, .. } => format!("{question}(y or n) "),
            EmacsPrompt::SaveSome { queue, at, .. } => save_question(self, queue[*at]),
            EmacsPrompt::ReadFile { prompt, typed, .. } => format!("{prompt}{typed}"),
            EmacsPrompt::LargeFile { question, .. } => question.clone(),
        };
        self.status.echo(text);
        self.emacs_prompt = Some(p);
    }

    /// Ask `question` with `yes-or-no-p` (or `y-or-n-p` when `short`).
    fn ask_yes(&mut self, question: String, short: bool, then: Then) {
        if short {
            self.ask(EmacsPrompt::YOrN { question, then });
        } else {
            self.ask(EmacsPrompt::YesOrNo {
                question,
                typed: String::new(),
                then,
            });
        }
    }

    /// Run an Emacs action the engine resolved.
    pub(crate) fn emacs_action(&mut self, a: EmacsAction, frame: &FrameCtx) {
        let _ = frame;
        match a {
            EmacsAction::KillTerminal => {
                let queue = self.buffers_needing_save(true);
                self.save_some(
                    queue,
                    Then::Kill {
                        stage: 1,
                        asked: false,
                    },
                );
            }
            EmacsAction::SaveSomeBuffers => {
                let queue = self.buffers_needing_save(false);
                if queue.is_empty() {
                    self.status.echo("(No files need saving)");
                } else {
                    self.save_some(queue, Then::Done);
                }
            }
            EmacsAction::RevertBuffer => self.revert_buffer(false),
            EmacsAction::RevertBufferQuick => self.revert_buffer(true),
            EmacsAction::ReadOnlyMode => self.read_only_mode(),
            EmacsAction::FindSiblingFile => self.find_sibling_file(),
            EmacsAction::WriteFile => {
                let prompt = EmacsPrompt::ReadFile {
                    prompt: "Write file: ".into(),
                    typed: default_dir(),
                    purpose: ReadFor::WriteFile,
                };
                self.ask(prompt);
            }
            EmacsAction::FindFile => {
                let dir = std::env::current_dir()
                    .map(|d| format!("{}/", d.display()))
                    .unwrap_or_default();
                let home = std::env::var("HOME").unwrap_or_default();
                let shown = match dir.strip_prefix(&home) {
                    Some(rest) if !home.is_empty() => format!("~{rest}"),
                    _ => dir,
                };
                self.ask(EmacsPrompt::ReadFile {
                    prompt: "Find file: ".into(),
                    typed: shown,
                    purpose: ReadFor::FindFile { confirmed: false },
                });
            }
        }
    }

    /// `files--buffers-needing-to-be-saved`: the modified buffers with a file, and (with `pred_t`, or
    /// `buffer-offer-save` `always`) those without one that `buffer-offer-save` offers and are not empty;
    /// without `pred_t`, `save-some-buffers-default-predicate` filters them.
    fn buffers_needing_save(&self, pred_t: bool) -> Vec<DocumentId> {
        let vars = self.emacs_vars();
        let root = (!pred_t && vars.save_some_buffers_default_predicate_root).then(project_root);
        self.ws
            .buffers()
            .into_iter()
            .filter(|b| b.modified && !self.terminals.contains_key(&b.id))
            .filter(|b| match self.files.get(&b.id) {
                Some(bf) => root.as_ref().is_none_or(|r| {
                    std::path::absolute(&bf.path)
                        .ok()
                        .and_then(|p| p.parent().map(|d| d.starts_with(r)))
                        .unwrap_or(false)
                }),
                None => {
                    let offer = vars.buffer_offer_save.as_str();
                    let size = self.ws.document(b.id).map_or(0, |d| d.len());
                    offer == "always" || (pred_t && offer == "t" && size > 0)
                }
            })
            .map(|b| b.id)
            .collect()
    }

    fn save_some(&mut self, queue: Vec<DocumentId>, then: Then) {
        if queue.is_empty() {
            self.emacs_then(then, true);
        } else {
            self.ask(EmacsPrompt::SaveSome {
                queue,
                at: 0,
                saved: 0,
                then,
            });
        }
    }

    /// Save buffer `id` (`save-buffer`); a buffer without a file reads one first. Returns false while a
    /// file name is being read.
    fn save_one(&mut self, id: DocumentId, rest: EmacsPrompt) -> bool {
        if !self.files.contains_key(&id) {
            self.ask(EmacsPrompt::ReadFile {
                prompt: "File to save in: ".into(),
                typed: String::new(),
                purpose: ReadFor::SaveAs {
                    buf: id,
                    rest: Box::new(rest),
                },
            });
            return false;
        }
        let Session {
            ws, files, status, ..
        } = self;
        crate::app::abandon::write_buffer(ws, files, id, status);
        true
    }

    /// Go on after save-some-buffers' question at `at`.
    fn save_some_next(&mut self, queue: Vec<DocumentId>, at: usize, saved: usize, then: Then) {
        if at + 1 < queue.len() {
            self.ask(EmacsPrompt::SaveSome {
                queue,
                at: at + 1,
                saved,
                then,
            });
        } else {
            self.status.clear();
            self.emacs_then(then, true);
        }
    }

    /// Continue `then` after a question answered `yes` (`ok`) or `no`.
    fn emacs_then(&mut self, then: Then, ok: bool) {
        if !ok {
            return;
        }
        match then {
            Then::Done => {}
            Then::Revert => {
                let id = self.ws.focused_buffer();
                self.revert_now(id);
            }
            Then::RevertBuffer(id) => self.revert_now(id),
            Then::WriteFile(name) => self.write_file(&name),
            Then::Kill { stage, asked } => self.kill_stage(stage, asked),
        }
    }

    /// `save-buffers-kill-emacs` after its save-some-buffers.
    fn kill_stage(&mut self, stage: u8, asked: bool) {
        let vars = self.emacs_vars();
        if stage <= 1 {
            let unsaved = self
                .ws
                .buffers()
                .iter()
                .any(|b| b.modified && self.files.contains_key(&b.id));
            if unsaved {
                return self.ask_yes(
                    "Modified buffers exist; exit anyway? ".into(),
                    false,
                    Then::Kill {
                        stage: 2,
                        asked: true,
                    },
                );
            }
        }
        if stage <= 2 && vars.confirm_kill_processes {
            let running = self.terminals.values().any(|t| t.is_running());
            if running {
                return self.ask_yes(
                    "Active processes exist; kill them and exit anyway? ".into(),
                    false,
                    Then::Kill {
                        stage: 3,
                        asked: true,
                    },
                );
            }
        }
        if stage <= 3 && !asked {
            match vars.confirm_kill_emacs.as_str() {
                "yes-or-no-p" | "y-or-n-p" => {
                    return self.ask_yes(
                        "Really exit Emacs? ".into(),
                        vars.confirm_kill_emacs == "y-or-n-p",
                        Then::Kill {
                            stage: 4,
                            asked: true,
                        },
                    );
                }
                _ => {}
            }
        }
        self.quit = true;
    }

    /// `revert-buffer` (`quick`: `revert-buffer-quick`).
    fn revert_buffer(&mut self, quick: bool) {
        let id = self.ws.focused_buffer();
        let Some(path) = self.files.get(&id).map(|bf| bf.path.clone()) else {
            self.status
                .error("Buffer does not seem to be associated with any file");
            return;
        };
        let vars = self.emacs_vars();
        let modified = self.ws.focused().doc.is_modified();
        let file = std::path::absolute(&path).unwrap_or(path);
        if quick && !modified {
            self.revert_now(id);
            self.status.echo("Reverted buffer");
            return;
        }
        let shown = file.display().to_string();
        if !modified
            && vars
                .revert_without_query
                .iter()
                .any(|re| emacs_match(re, &shown))
        {
            self.revert_now(id);
            return;
        }
        let question = if modified {
            format!("Discard edits and reread from {shown}? ")
        } else {
            format!("Revert buffer from file {shown}? ")
        };
        self.ask_yes(
            question,
            quick && vars.revert_buffer_quick_short_answers,
            Then::Revert,
        );
    }

    /// Re-read buffer `id` from its file as one undoable change, marked unmodified.
    fn revert_now(&mut self, id: DocumentId) {
        let Some(path) = self.files.get(&id).map(|bf| bf.path.clone()) else {
            return;
        };
        match persist::read_file(&path) {
            Ok(raw) => {
                persist::disk::saw(&path, &raw);
                persist::backup::visited(&path);
                let fmt = self.files.write.read_format(&raw);
                self.ws.reload_undoable(id, fmt.to_buffer(&raw));
                if let Some(bf) = self.files.get_mut(&id) {
                    bf.fmt = fmt;
                    bf.reloads = bf.reloads.saturating_add(1);
                }
                persist::journal::discard(Some(&path));
                self.disk_changed.remove(&id);
                self.disk_warned.remove(&id);
            }
            Err(e) => self
                .status
                .error(format!("Opening input file: {e}, {}", path.display())),
        }
    }

    /// `read-only-mode` (`C-x C-q`): toggle `buffer-read-only`; with `view-read-only`, turning it on enters
    /// View mode and turning it off leaves it.
    fn read_only_mode(&mut self) {
        let id = self.ws.focused_buffer();
        let on = self.ws.focused().doc.is_modifiable();
        self.ws.focused_doc_mut().set_modifiable(!on);
        if on {
            self.status.echo("Read-Only mode enabled in current buffer");
            if self.emacs_vars().view_read_only {
                self.view_mode.insert(id, true);
            }
        } else {
            self.status
                .echo("Read-Only mode disabled in current buffer");
            self.view_mode.remove(&id);
        }
    }

    /// A key in View mode (`view-mode-map`), or `None` when View mode does not bind it.
    pub(crate) fn view_key(&mut self, key: KeyEvent, frame: &FrameCtx) -> bool {
        let id = self.ws.focused_buffer();
        let Some(&from_read_only) = self.view_mode.get(&id) else {
            return false;
        };
        let rows = usize::from(frame.text_rows.max(3));
        let page = rows.saturating_sub(2).max(1);
        let half = (rows / 2).max(1);
        let shift_space = key.code == KeyCode::Char(' ') && key.modifiers == KeyModifiers::SHIFT;
        let mv = |s: &mut Session, down: bool, n: usize| {
            let cmd = if down {
                Command::MoveDown
            } else {
                Command::MoveUp
            };
            let Session {
                ws,
                files,
                recorded,
                status,
                quit,
                ..
            } = s;
            for _ in 0..n {
                crate::app::dispatch::run_cmd(cmd.clone(), ws, files, recorded, status, quit);
            }
        };
        if shift_space || key.code == KeyCode::Backspace && key.modifiers.is_empty() {
            mv(self, false, page);
        } else if plain(key, ' ') {
            mv(self, true, page);
        } else if plain(key, 'd') {
            mv(self, true, half);
        } else if plain(key, 'u') {
            mv(self, false, half);
        } else if key.code == KeyCode::Enter && key.modifiers.is_empty() {
            mv(self, true, 1);
        } else if plain(key, '<') || plain(key, '>') {
            let cmd = Command::EmacsBufferEdge {
                start: plain(key, '<'),
            };
            let Session {
                ws,
                files,
                recorded,
                status,
                quit,
                ..
            } = self;
            crate::app::dispatch::run_cmd(cmd, ws, files, recorded, status, quit);
        } else if plain(key, 'q') || plain(key, 'e') || plain(key, 'E') {
            // `q` (`View-quit`) gives a buffer View mode made read-only its writability back; `e`
            // (`View-exit`) keeps it read-only; `E` (`View-exit-and-edit`) makes it writable.
            self.view_mode.remove(&id);
            if plain(key, 'E') || (plain(key, 'q') && from_read_only) {
                self.ws.focused_doc_mut().set_modifiable(true);
            }
            self.status.clear();
        } else {
            return false;
        }
        true
    }

    /// `find-file` of `name`: wildcards visit every match (`find-file-wildcards`), `directory-abbrev-alist`
    /// and `find-file-visit-truename` shape the name, and past `large-file-warning-threshold` ask first
    /// (`abort-if-file-too-large`).
    fn find_file(&mut self, name: &str) {
        let name = crate::persist::backup::expand(name.trim());
        if name.is_empty() {
            return;
        }
        let vars = self.emacs_vars();
        if vars.find_file_wildcards && !std::path::Path::new(&name).exists() {
            let matches = expand_wildcards(&name);
            if !matches.is_empty() {
                // Emacs visits them all and shows the first.
                for m in matches.iter().rev() {
                    self.find_file_one(m);
                }
                return;
            }
        }
        self.find_file_one(&name);
    }

    fn find_file_one(&mut self, name: &str) {
        let vars = self.emacs_vars();
        let mut name = name.to_string();
        for (from, to) in &vars.directory_abbrev_alist {
            if let Some(end) = emacs_match_end_at_start(from, &name) {
                name = format!("{to}{}", &name[end..]);
            }
        }
        if vars.find_file_visit_truename {
            if let Ok(t) = std::fs::canonicalize(&name) {
                name = t.display().to_string();
            }
        }
        let path = std::path::PathBuf::from(&name);
        let size = std::fs::metadata(&path).map_or(0, |m| m.len());
        let vars = self.emacs_vars();
        let visited = crate::app::dispatch::buffer_for_path(&self.files, &path).is_some();
        if !visited
            && vars
                .large_file_warning_threshold
                .is_some_and(|limit| size > limit)
        {
            let base = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let question = format!(
                "File {base} is large ({}), really open? ([Y]es, [N]o, [L]iterally, [?]): ",
                human_size(size as f64)
            );
            self.ask(EmacsPrompt::LargeFile { name, question });
            return;
        }
        self.visit(&name, false);
    }

    /// Visit `name` (`literally`: `find-file-literally`, read with `'binary'`): switch to its buffer, asking
    /// or telling about a file changed on disk, and warn past `out-of-memory-warning-percentage`.
    fn visit(&mut self, name: &str, literally: bool) {
        let path = std::path::PathBuf::from(name);
        let existing = crate::app::dispatch::buffer_for_path(&self.files, &path);
        let size = std::fs::metadata(&path).map_or(0, |m| m.len());
        let new = std::fs::symlink_metadata(&path).is_err();
        // `find-file-existing-other-name`: a buffer visiting the same file under another name is used, and
        // Emacs says so unless `find-file-suppress-same-file-warnings`.
        let same_file = existing.and_then(|id| {
            let other = self.files.get(&id)?.path.clone();
            let a = std::path::absolute(&path).ok()?;
            let b = std::path::absolute(&other).ok()?;
            (a != b).then(|| (a.display().to_string(), b.display().to_string()))
        });
        {
            let Session {
                ws,
                files,
                highlighters,
                status,
                ..
            } = self;
            let opened = crate::app::dispatch::open_file_with(name, ws, files, highlighters, |f| {
                if literally {
                    crate::app::file_cmds::with_ff(
                        f,
                        crate::input::FileOpts {
                            ff: None,
                            bin: Some(true),
                        },
                    )
                } else {
                    f
                }
            });
            status.give(opened.status);
            if opened.id.is_none() {
                return;
            }
        }
        let Some(id) = existing else {
            self.status.clear();
            if new {
                self.status.echo("(New file)");
            }
            if let Some(warning) = out_of_memory_warning(size, &self.emacs_vars()) {
                self.status.warn(warning);
            }
            return;
        };
        self.status.clear();
        if let Some((a, b)) = same_file {
            if !self.emacs_vars().find_file_suppress_same_file_warnings {
                self.status.echo(format!("{a} and {b} are the same file"));
            }
        }
        let Some(bf) = self.files.get(&id) else {
            return;
        };
        if persist::disk::check(&bf.path) != persist::disk::State::Changed {
            return;
        }
        let vars = self.emacs_vars();
        let file = std::path::absolute(&bf.path).unwrap_or_else(|_| bf.path.clone());
        let base = file
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let modified = self.ws.document(id).is_some_and(|d| d.is_modified());
        let shown = file.display().to_string();
        if !modified
            && vars
                .revert_without_query
                .iter()
                .any(|re| emacs_match(re, &shown))
        {
            self.revert_now(id);
            self.status.echo(format!("Reverting file {shown}...done"));
        } else if !vars.query_about_changed_file {
            self.status.echo(format!(
                "File {base} changed on disk.  C-x x g to load new contents{}",
                if modified {
                    " and discard your edits"
                } else {
                    ""
                }
            ));
        } else {
            let question = if modified {
                format!("File {base} changed on disk.  Discard your edits? ")
            } else {
                format!("File {base} changed on disk.  Reread from disk? ")
            };
            self.ask_yes(question, false, Then::RevertBuffer(id));
        }
    }

    /// The key that answered an Emacs question.
    pub(crate) fn answer_emacs(&mut self, p: EmacsPrompt, key: KeyEvent) {
        if ctrl_g(key) {
            self.status.echo("Quit");
            return;
        }
        match p {
            EmacsPrompt::YesOrNo {
                question,
                mut typed,
                then,
            } => match key.code {
                KeyCode::Enter => match typed.as_str() {
                    "yes" => {
                        self.status.clear();
                        self.emacs_then(then, true);
                    }
                    "no" => self.status.clear(),
                    _ => {
                        self.status.echo("Please answer yes or no.");
                        self.emacs_prompt = Some(EmacsPrompt::YesOrNo {
                            question,
                            typed: String::new(),
                            then,
                        });
                    }
                },
                KeyCode::Backspace => {
                    typed.pop();
                    self.ask(EmacsPrompt::YesOrNo {
                        question,
                        typed,
                        then,
                    });
                }
                KeyCode::Char(c)
                    if !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                {
                    typed.push(c);
                    self.ask(EmacsPrompt::YesOrNo {
                        question,
                        typed,
                        then,
                    });
                }
                _ => self.ask(EmacsPrompt::YesOrNo {
                    question,
                    typed,
                    then,
                }),
            },
            EmacsPrompt::YOrN { question, then } => {
                if plain(key, 'y') || plain(key, 'Y') || plain(key, ' ') {
                    self.status.clear();
                    self.emacs_then(then, true);
                } else if plain(key, 'n') || plain(key, 'N') || key.code == KeyCode::Backspace {
                    self.status.clear();
                } else {
                    self.status
                        .echo(format!("Please answer y or n.  {question}(y or n) "));
                    self.emacs_prompt = Some(EmacsPrompt::YOrN { question, then });
                }
            }
            EmacsPrompt::SaveSome {
                queue,
                at,
                saved,
                then,
            } => {
                let id = queue[at];
                let here = EmacsPrompt::SaveSome {
                    queue: queue.clone(),
                    at,
                    saved,
                    then: then.clone(),
                };
                if plain(key, 'y') || plain(key, ' ') {
                    let next = EmacsPrompt::SaveSome {
                        queue: queue.clone(),
                        at,
                        saved: saved + 1,
                        then: then.clone(),
                    };
                    if self.save_one(id, next) {
                        self.save_some_next(queue, at, saved + 1, then);
                    }
                } else if plain(key, 'n') || key.code == KeyCode::Backspace {
                    self.save_some_next(queue, at, saved, then);
                } else if key.code == KeyCode::Char('~')
                    && key.modifiers.contains(KeyModifiers::ALT)
                {
                    if let Some(d) = self.ws.document_mut(id) {
                        d.mark_saved();
                    }
                    self.save_some_next(queue, at, saved, then);
                } else if plain(key, '!') {
                    for &b in &queue[at..] {
                        if self.files.contains_key(&b) {
                            let Session {
                                ws, files, status, ..
                            } = self;
                            crate::app::abandon::write_buffer(ws, files, b, status);
                        }
                    }
                    self.status.clear();
                    self.emacs_then(then, true);
                } else if plain(key, '.') {
                    let done = EmacsPrompt::SaveSome {
                        queue: vec![id],
                        at: 0,
                        saved: saved + 1,
                        then: then.clone(),
                    };
                    if self.save_one(id, done) {
                        self.status.clear();
                        self.emacs_then(then, true);
                    }
                } else if plain(key, 'q') || key.code == KeyCode::Enter {
                    self.status.clear();
                    self.emacs_then(then, true);
                } else {
                    // `C-r`, `C-f`, `d` and `C-h` are Emacs's view, diff and help actions, which ruse does
                    // not offer here; Emacs says the same for a key it does not know.
                    self.ask(here);
                }
            }
            EmacsPrompt::LargeFile { name, question } => {
                if plain(key, 'y') || plain(key, 'Y') {
                    self.status.clear();
                    self.visit(&name, false);
                } else if plain(key, 'l') || plain(key, 'L') {
                    self.status.clear();
                    self.visit(&name, true);
                } else if plain(key, 'n') || plain(key, 'N') {
                    self.status.error("Aborted");
                } else {
                    self.ask(EmacsPrompt::LargeFile { name, question });
                }
            }
            EmacsPrompt::ReadFile {
                prompt,
                mut typed,
                purpose,
            } => match key.code {
                KeyCode::Enter => {
                    self.status.clear();
                    match purpose {
                        ReadFor::FindFile { confirmed } => {
                            // `confirm-nonexistent-file-or-buffer` t: a new file's name needs a second
                            // `RET` (`[Confirm]`); `after-completion` asks only after a completion, which
                            // this minibuffer does not offer.
                            let name = crate::persist::backup::expand(typed.trim());
                            let fresh = !name.is_empty()
                                && !std::path::Path::new(&name).exists()
                                && expand_wildcards(&name).is_empty();
                            if fresh
                                && !confirmed
                                && self.emacs_vars().confirm_nonexistent_file_or_buffer == "t"
                            {
                                let shown = format!("{prompt}{typed} [Confirm]");
                                self.ask(EmacsPrompt::ReadFile {
                                    prompt,
                                    typed,
                                    purpose: ReadFor::FindFile { confirmed: true },
                                });
                                self.status.echo(shown);
                            } else {
                                self.find_file(&typed);
                            }
                        }
                        ReadFor::WriteFile => {
                            let name = crate::persist::backup::expand(typed.trim());
                            if name.is_empty() {
                                return;
                            }
                            if std::path::Path::new(&name).exists() {
                                let base = std::path::Path::new(&name)
                                    .file_name()
                                    .map(|n| n.to_string_lossy().into_owned())
                                    .unwrap_or_default();
                                self.ask_yes(
                                    format!("File {base} exists; overwrite? "),
                                    true,
                                    Then::WriteFile(name),
                                );
                            } else {
                                self.write_file(&name);
                            }
                        }
                        ReadFor::SaveAs { buf, rest } => {
                            let name = crate::persist::backup::expand(typed.trim());
                            self.write_new_file(buf, &name);
                            match *rest {
                                EmacsPrompt::SaveSome {
                                    queue,
                                    at,
                                    saved,
                                    then,
                                } if queue.len() > 1 || at > 0 => {
                                    self.save_some_next(queue, at, saved, then)
                                }
                                EmacsPrompt::SaveSome { then, .. } => self.emacs_then(then, true),
                                other => self.ask(other),
                            }
                        }
                    }
                }
                KeyCode::Backspace => {
                    typed.pop();
                    self.ask(EmacsPrompt::ReadFile {
                        prompt,
                        typed,
                        purpose: unconfirmed(purpose),
                    });
                }
                KeyCode::Char(c)
                    if !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                {
                    typed.push(c);
                    self.ask(EmacsPrompt::ReadFile {
                        prompt,
                        typed,
                        purpose: unconfirmed(purpose),
                    });
                }
                _ => self.ask(EmacsPrompt::ReadFile {
                    prompt,
                    typed,
                    purpose,
                }),
            },
        }
    }

    /// `write-file` (`C-x C-w`): the focused buffer is written to `name`, which becomes its file. Under
    /// `change-major-mode-with-file-name` nil it keeps its highlighting.
    fn write_file(&mut self, name: &str) {
        let id = self.ws.focused_buffer();
        let keep = (!self.emacs_vars().change_major_mode_with_file_name)
            .then(|| self.highlighters.remove(&id))
            .flatten();
        let Session {
            ws,
            files,
            highlighters,
            status,
            ..
        } = self;
        crate::app::file_cmds::ex_saveas(
            true,
            name,
            crate::input::FileOpts::default(),
            ws,
            files,
            highlighters,
            status,
        );
        if let Some(h) = keep {
            self.highlighters.insert(id, h);
        }
    }

    /// `find-sibling-file`: the first existing file a `find-sibling-rules` rule names from the visited one.
    fn find_sibling_file(&mut self) {
        let id = self.ws.focused_buffer();
        let Some(path) = self.files.get(&id).map(|bf| bf.path.clone()) else {
            self.status.error("Not visiting a file");
            return;
        };
        let rules = self.emacs_vars().find_sibling_rules;
        if rules.is_empty() {
            self.status
                .error("The `find-sibling-rules' variable has not been configured");
            return;
        }
        let file = std::path::absolute(&path)
            .unwrap_or(path)
            .display()
            .to_string();
        let found = sibling_files(&file, &rules);
        match found.first() {
            Some(f) => {
                let f = f.clone();
                self.find_file(&f);
            }
            None => self.status.error("Couldn't find any sibling files"),
        }
    }

    /// `write-file` of the buffer `buf`, which has no file, to `name`.
    fn write_new_file(&mut self, buf: DocumentId, name: &str) {
        if name.is_empty() {
            return;
        }
        let focus = self.ws.focused_buffer();
        if focus != buf {
            self.ws.focus_buffer(buf);
        }
        let spec = crate::input::WriteSpec {
            range: None,
            bang: false,
            target: crate::input::WriteTarget::File(Some(name.to_string())),
            opts: crate::input::FileOpts::default(),
            if_modified: false,
            quit: false,
        };
        let Session {
            ws,
            files,
            highlighters,
            status,
            ..
        } = self;
        crate::app::file_cmds::ex_write(&spec, ws, files, highlighters, status);
        if focus != buf {
            self.ws.focus_buffer(focus);
        }
    }
}

/// Emacs's `file-size-human-readable-iec`: powers of 1024, one decimal below 10 when it is not whole
/// (`1.5 KiB`, `10 MiB`, `500 B`).
pub(crate) fn human_size(mut size: f64) -> String {
    let units = ["B", "KiB", "MiB", "GiB", "TiB", "PiB", "EiB", "ZiB", "YiB"];
    let mut i = 0;
    while size >= 1024.0 && i + 1 < units.len() {
        size /= 1024.0;
        i += 1;
    }
    let frac = size % 1.0;
    if size < 10.0 && (0.05..0.95).contains(&frac) {
        format!("{size:.1} {}", units[i])
    } else {
        format!("{size:.0} {}", units[i])
    }
}

/// Emacs's `warn-maybe-out-of-memory`: the warning when a file of `size` bytes is over
/// `out-of-memory-warning-percentage` of the free memory (RAM and swap), `None` otherwise or when the free
/// memory cannot be read.
fn out_of_memory_warning(size: u64, vars: &EmacsFiles) -> Option<String> {
    let pct = vars.out_of_memory_warning_percentage?;
    if size == 0 {
        return None;
    }
    let free_kib = free_memory_kib()? as f64;
    if (size as f64 / 1024.0) <= free_kib * pct as f64 / 100.0 {
        return None;
    }
    Some(format!(
        "Warning (emacs): You are trying to open a file whose size ({})\nexceeds the {pct}% of currently \
         available free memory ({}).\nIf that fails, try to open it with `find-file-literally'\n(but note \
         that some characters might be displayed incorrectly).",
        human_size(size as f64),
        human_size(free_kib * 1024.0)
    ))
}

/// Free RAM plus free swap in KiB, as Emacs's `memory-info` counts it.
#[cfg(target_os = "linux")]
fn free_memory_kib() -> Option<u64> {
    // SAFETY: `sysinfo` fills the zeroed struct it is given.
    let mut info: libc::sysinfo = unsafe { std::mem::zeroed() };
    if unsafe { libc::sysinfo(&mut info) } != 0 {
        return None;
    }
    let unit = u64::from(info.mem_unit.max(1));
    Some((info.freeram as u64 + info.freeswap as u64) * unit / 1024)
}

#[cfg(target_os = "macos")]
fn free_memory_kib() -> Option<u64> {
    fn sysctl<T: Default>(name: &str) -> Option<T> {
        let name = std::ffi::CString::new(name).ok()?;
        let mut out = T::default();
        let mut len = std::mem::size_of::<T>();
        // SAFETY: `out` is `len` bytes, and the name is NUL-terminated.
        let rc = unsafe {
            libc::sysctlbyname(
                name.as_ptr(),
                (&mut out as *mut T).cast(),
                &mut len,
                std::ptr::null_mut(),
                0,
            )
        };
        (rc == 0).then_some(out)
    }
    let pages: u32 = sysctl("vm.page_free_count")?;
    // SAFETY: `sysconf` has no preconditions.
    let page = u64::try_from(unsafe { libc::sysconf(libc::_SC_PAGESIZE) }).ok()?;
    let swap: libc::xsw_usage = sysctl_swap()?;
    Some((u64::from(pages) * page + swap.xsu_avail) / 1024)
}

#[cfg(target_os = "macos")]
fn sysctl_swap() -> Option<libc::xsw_usage> {
    let name = std::ffi::CString::new("vm.swapusage").ok()?;
    // SAFETY: zeroed is a valid `xsw_usage`.
    let mut out: libc::xsw_usage = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::xsw_usage>();
    // SAFETY: `out` is `len` bytes, and the name is NUL-terminated.
    let rc = unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            (&mut out as *mut libc::xsw_usage).cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    (rc == 0).then_some(out)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn free_memory_kib() -> Option<u64> {
    None
}

/// The default directory a file prompt starts from, `~`-abbreviated as Emacs shows it.
fn default_dir() -> String {
    let dir = std::env::current_dir()
        .map(|d| format!("{}/", d.display()))
        .unwrap_or_default();
    let home = std::env::var("HOME").unwrap_or_default();
    match dir.strip_prefix(&home) {
        Some(rest) if !home.is_empty() => format!("~{rest}"),
        _ => dir,
    }
}

/// A file prompt's purpose after an edit: a confirmed new name needs confirming again.
fn unconfirmed(p: ReadFor) -> ReadFor {
    match p {
        ReadFor::FindFile { .. } => ReadFor::FindFile { confirmed: false },
        other => other,
    }
}

/// `file-expand-wildcards` for `find-file-wildcards`: the files matching a name whose last component has
/// `*`, `?` or `[...]`, sorted; empty for a name without wildcards or with no match.
fn expand_wildcards(name: &str) -> Vec<String> {
    let path = std::path::Path::new(name);
    let Some(base) = path.file_name().map(|b| b.to_string_lossy().into_owned()) else {
        return Vec::new();
    };
    if !base.contains(['*', '?', '[']) {
        return Vec::new();
    }
    let dir = path
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
        .unwrap_or(std::path::Path::new("."));
    let mut out: Vec<String> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let n = e.file_name().to_string_lossy().into_owned();
            // A wildcard does not match a leading dot, as the shell's.
            let hidden = n.starts_with('.') && !base.starts_with('.');
            (!hidden && crate::persist::backup::glob_match(base.as_bytes(), n.as_bytes()))
                .then(|| dir.join(n).display().to_string())
        })
        .collect();
    out.sort();
    out
}

/// `find-sibling-file-search`: for each rule whose MATCH matches `file`, its EXPANSION with `\N` and `\&`
/// replaced by the match's groups, read relative to the directory before the match; the existing ones,
/// in rule order, without `file` itself. An EXPANSION is a file name here, not a regexp.
fn sibling_files(file: &str, rules: &[(String, String)]) -> Vec<String> {
    let mut out = Vec::new();
    for (pattern, expansion) in rules {
        let Ok(re) =
            ruse_core::pattern::Regex::compile(pattern, ruse_core::pattern::Options::default())
        else {
            continue;
        };
        let Some(groups) = re.groups(file) else {
            continue;
        };
        let group = |i: usize| {
            groups
                .get(i)
                .copied()
                .flatten()
                .map_or("", |(a, b)| &file[a..b])
        };
        let mut name = String::new();
        let mut chars = expansion.chars().peekable();
        while let Some(c) = chars.next() {
            match (c, chars.peek()) {
                ('\\', Some('&')) => {
                    chars.next();
                    name.push_str(group(0));
                }
                ('\\', Some(d)) if d.is_ascii_digit() => {
                    let i = d.to_digit(10).unwrap_or(0) as usize;
                    chars.next();
                    name.push_str(group(i));
                }
                _ => name.push(c),
            }
        }
        let start = groups[0].map_or(0, |(a, _)| a);
        let candidate = std::path::Path::new(&file[..start]).join(&name);
        let shown = candidate.display().to_string();
        if shown != file && candidate.is_file() && !out.contains(&shown) {
            out.push(shown);
        }
    }
    out
}

/// Where a `directory-abbrev-alist` FROM matches at the start of `name`, its end.
fn emacs_match_end_at_start(re: &str, name: &str) -> Option<usize> {
    let re = ruse_core::pattern::Regex::compile(re, ruse_core::pattern::Options::default()).ok()?;
    let m = re.find_at(name, ruse_core::BytePos(0))?;
    (m.start.0 == 0).then_some(m.end.0)
}

/// `project-current`'s root for `save-some-buffers-root`: the nearest directory above the working one
/// holding `.git` (the VC backend project.el uses), else the working directory.
fn project_root() -> std::path::PathBuf {
    let cwd = std::env::current_dir().unwrap_or_default();
    cwd.ancestors()
        .find(|d| d.join(".git").exists())
        .map_or_else(|| cwd.clone(), std::path::Path::to_path_buf)
}

/// An Emacs regexp of `revert-without-query` against a file name, read by ruse's Vim-magic engine (which
/// spells groups and alternation as Emacs does).
fn emacs_match(re: &str, name: &str) -> bool {
    ruse_core::pattern::Regex::compile(re, ruse_core::pattern::Options::default())
        .is_ok_and(|r| r.find_at(name, ruse_core::BytePos(0)).is_some())
}
