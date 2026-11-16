//! `save-some-buffers` (`C-x s`) and the checks `save-buffers-kill-emacs` (`C-x C-c`) runs after it
//! (#619, #1503): which buffers are offered, the `map-y-or-n-p` question and its answers, and the
//! `Modified buffers exist`, `Active processes exist` and `Really exit Emacs?` questions.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ruse_core::DocumentId;

use super::{plain, EmacsPrompt, ReadFor, Then};
use crate::app::session::Session;

/// The `save-some-buffers` question for `id`.
pub(super) fn save_question(s: &Session, id: DocumentId) -> String {
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

impl Session {
    /// `files--buffers-needing-to-be-saved`: the modified buffers with a file, and (with `pred_t`, or
    /// `buffer-offer-save` `always`) those without one that `buffer-offer-save` offers and are not empty;
    /// without `pred_t`, `save-some-buffers-default-predicate` filters them.
    pub(super) fn buffers_needing_save(&self, pred_t: bool) -> Vec<DocumentId> {
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

    pub(super) fn save_some(&mut self, queue: Vec<DocumentId>, then: Then) {
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
    pub(super) fn save_some_next(
        &mut self,
        queue: Vec<DocumentId>,
        at: usize,
        saved: usize,
        then: Then,
    ) {
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

    /// `save-buffers-kill-emacs` after its save-some-buffers.
    pub(super) fn kill_stage(&mut self, stage: u8, asked: bool) {
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

    /// `save-some-buffers`: one buffer at a time, `!` for the rest, `.` for this one only, `q` to stop.
    pub(super) fn answer_save_some(
        &mut self,
        queue: Vec<DocumentId>,
        at: usize,
        saved: usize,
        then: Then,
        key: KeyEvent,
    ) {
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
        } else if key.code == KeyCode::Char('~') && key.modifiers.contains(KeyModifiers::ALT) {
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
}

/// `project-current`'s root for `save-some-buffers-root`: the nearest directory above the working one
/// holding `.git` (the VC backend project.el uses), else the working directory.
fn project_root() -> std::path::PathBuf {
    let cwd = std::env::current_dir().unwrap_or_default();
    cwd.ancestors()
        .find(|d| d.join(".git").exists())
        .map_or_else(|| cwd.clone(), std::path::Path::to_path_buf)
}
