//! `revert-buffer` and `revert-buffer-quick` (`C-x x g`) under `revert-without-query` (#619, #1503),
//! and the re-read of a buffer from its file that they and find-file's changed-on-disk question end in.

use ruse_core::DocumentId;

use super::Then;
use crate::app::session::Session;
use crate::persist;

impl Session {
    /// `revert-buffer` (`quick`: `revert-buffer-quick`).
    pub(super) fn revert_buffer(&mut self, quick: bool) {
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
    pub(super) fn revert_now(&mut self, id: DocumentId) {
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
}

/// An Emacs regexp of `revert-without-query` against a file name, read by ruse's Vim-magic engine (which
/// spells groups and alternation as Emacs does).
pub(super) fn emacs_match(re: &str, name: &str) -> bool {
    ruse_core::pattern::Regex::compile(re, ruse_core::pattern::Options::default())
        .is_ok_and(|r| r.find_at(name, ruse_core::BytePos(0)).is_some())
}
