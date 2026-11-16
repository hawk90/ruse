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
//!
//! This file holds the questions themselves: the prompt state, asking, dispatching the actions and the
//! answers, the minibuffer file name and `write-file`. The commands live in children (#1503):
//! `save_some` (save-some-buffers and the `C-x C-c` checks), `revert`, `view_mode` (read-only-mode
//! and View mode), `visit` (find-file, the large-file question, find-sibling-file) and `memory`
//! (sizes and the out-of-memory warning).

use super::*;
use crate::app::write_opts::EmacsFiles;
use crate::input::EmacsAction;

mod memory;
mod revert;
mod save_some;
mod view_mode;
mod visit;

use save_some::save_question;
use visit::expand_wildcards;

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
        self.ed.files.write.emacs.clone().unwrap_or_default()
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
        self.ed.status.echo(text);
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
                    self.ed.status.echo("(No files need saving)");
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
                self.ask(EmacsPrompt::ReadFile {
                    prompt: "Find file: ".into(),
                    typed: default_dir(),
                    purpose: ReadFor::FindFile { confirmed: false },
                });
            }
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
                let id = self.ed.ws.focused_buffer();
                self.revert_now(id);
            }
            Then::RevertBuffer(id) => self.revert_now(id),
            Then::WriteFile(name) => self.write_file(&name),
            Then::Kill { stage, asked } => self.kill_stage(stage, asked),
        }
    }

    /// The key that answered an Emacs question.
    pub(crate) fn answer_emacs(&mut self, p: EmacsPrompt, key: KeyEvent) {
        if ctrl_g(key) {
            self.ed.status.echo("Quit");
            return;
        }
        match p {
            EmacsPrompt::YesOrNo {
                question,
                typed,
                then,
            } => self.answer_yes_or_no(question, typed, then, key),
            EmacsPrompt::YOrN { question, then } => self.answer_y_or_n(question, then, key),
            EmacsPrompt::SaveSome {
                queue,
                at,
                saved,
                then,
            } => self.answer_save_some(queue, at, saved, then, key),
            EmacsPrompt::LargeFile { name, question } => {
                self.answer_large_file(name, question, key)
            }
            EmacsPrompt::ReadFile {
                prompt,
                typed,
                purpose,
            } => self.answer_read_file(prompt, typed, purpose, key),
        }
    }

    /// `yes-or-no-p`: type `yes` or `no`, then `RET`.
    fn answer_yes_or_no(&mut self, question: String, mut typed: String, then: Then, key: KeyEvent) {
        match key.code {
            KeyCode::Enter => match typed.as_str() {
                "yes" => {
                    self.ed.status.clear();
                    self.emacs_then(then, true);
                }
                "no" => self.ed.status.clear(),
                _ => {
                    self.ed.status.echo("Please answer yes or no.");
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
        }
    }

    /// `y-or-n-p`: one key.
    fn answer_y_or_n(&mut self, question: String, then: Then, key: KeyEvent) {
        if plain(key, 'y') || plain(key, 'Y') || plain(key, ' ') {
            self.ed.status.clear();
            self.emacs_then(then, true);
        } else if plain(key, 'n') || plain(key, 'N') || key.code == KeyCode::Backspace {
            self.ed.status.clear();
        } else {
            self.ed
                .status
                .echo(format!("Please answer y or n.  {question}(y or n) "));
            self.emacs_prompt = Some(EmacsPrompt::YOrN { question, then });
        }
    }

    /// A file name read in the minibuffer: `find-file`, `write-file`, or the name a `save-some-buffers` buffer is saved as.
    fn answer_read_file(
        &mut self,
        prompt: String,
        mut typed: String,
        purpose: ReadFor,
        key: KeyEvent,
    ) {
        match key.code {
            KeyCode::Enter => {
                self.ed.status.clear();
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
                            self.ed.status.echo(shown);
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
        }
    }

    /// `write-file` (`C-x C-w`): the focused buffer is written to `name`, which becomes its file. Under
    /// `change-major-mode-with-file-name` nil it keeps its highlighting.
    fn write_file(&mut self, name: &str) {
        let id = self.ed.ws.focused_buffer();
        let keep = (!self.emacs_vars().change_major_mode_with_file_name)
            .then(|| self.highlighters.remove(&id))
            .flatten();
        let Session {
            ed: Editing {
                ws, files, status, ..
            },
            highlighters,
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

    /// `write-file` of the buffer `buf`, which has no file, to `name`.
    fn write_new_file(&mut self, buf: DocumentId, name: &str) {
        if name.is_empty() {
            return;
        }
        let focus = self.ed.ws.focused_buffer();
        if focus != buf {
            self.ed.ws.focus_buffer(buf);
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
            ed: Editing {
                ws, files, status, ..
            },
            highlighters,
            ..
        } = self;
        crate::app::file_cmds::ex_write(&spec, ws, files, highlighters, status);
        if focus != buf {
            self.ed.ws.focus_buffer(focus);
        }
    }
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
