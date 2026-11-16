//! Per-buffer file identity ([`BufferFile`], the [`Files`] registry) and the `'readonly'` state.

use std::collections::HashMap;
use std::path::PathBuf;

use ruse_core::{DocumentId, Mode, Revision, Workspace};

use crate::app::messages::Status;
use crate::persist;

/// One file buffer's on-disk identity: its path and the detected encoding/line-ending to restore on save
/// (F-008). Held frontend-side (core is IO-free), keyed by [`DocumentId`] in a [`Files`] registry.
pub(crate) struct BufferFile {
    pub(crate) path: PathBuf,
    pub(crate) fmt: persist::encoding::FileFormat,
    /// Opened read-only from the recovery prompt while another session edits the file (Vim's
    /// `'readonly'`): a write to it without `!` is `E45`.
    pub(crate) read_only: bool,
    /// How many times `:e`/`:e!` re-read the file into the buffer: a re-read is new text from disk and a
    /// fresh undo history, which a trace cannot record (#690, [`crate::app::trace_check`]).
    pub(crate) reloads: u32,
    /// Read past the large-file threshold: the degraded profile ([`super::mark_large`]).
    pub(crate) large: bool,
    /// Vim's `'swapfile'` (#619): the buffer keeps a recovery journal (ruse's swap file, RFC-0023).
    pub(crate) swapfile: bool,
}

/// Neovim's message for a write to a `'readonly'` buffer without `!`.
pub(crate) const E45: &str = "E45: 'readonly' option is set (add ! to override)";

/// Neovim's warning on the first change to a `'readonly'` buffer.
pub(crate) const W10: &str = "W10: Warning: Changing a readonly file";

/// The `'readonly'` state that is not a file buffer's own [`BufferFile::read_only`] (#1211): the flag of a
/// buffer without a file, which buffers gave W10 since `'readonly'` was last set (nvim's `b_did_warn`),
/// and `ruse -R` (nvim's readonly mode, which makes every file the session opens read-only).
#[derive(Default, Debug)]
pub(crate) struct ReadOnly {
    /// `ruse -R`: every file buffer starts read-only, until a `:set noro`.
    pub(crate) mode: bool,
    /// The file buffers the readonly mode already made read-only, so a `:setlocal noro` sticks.
    seen: std::collections::HashSet<DocumentId>,
    nofile: std::collections::HashSet<DocumentId>,
    warned: std::collections::HashSet<DocumentId>,
}

impl ReadOnly {
    /// Whether buffer `id` is `'readonly'`.
    pub(crate) fn get(&self, files: &Files, id: DocumentId) -> bool {
        files
            .get(&id)
            .map_or_else(|| self.nofile.contains(&id), |bf| bf.read_only)
    }

    /// Set or reset `'readonly'` on buffer `id`. Setting it lets the next change warn again (nvim).
    pub(crate) fn set(&mut self, files: &mut Files, id: DocumentId, on: bool) {
        match files.get_mut(&id) {
            Some(bf) => bf.read_only = on,
            None if on => {
                self.nofile.insert(id);
            }
            None => {
                self.nofile.remove(&id);
            }
        }
        if on {
            self.warned.remove(&id);
        }
    }

    /// In readonly mode, make every file buffer not seen yet read-only.
    pub(crate) fn follow(&mut self, files: &mut Files) {
        if !self.mode {
            return;
        }
        for (id, bf) in files.iter_mut() {
            if self.seen.insert(*id) {
                bf.read_only = true;
            }
        }
    }

    /// What a key may warn about: the focused buffer and its revision when it is read-only, unmodified,
    /// and has not warned yet, else `None`.
    pub(crate) fn watch(&self, ws: &Workspace, files: &Files) -> Option<(DocumentId, Revision)> {
        let id = ws.focused_buffer();
        let doc = ws.focused().doc;
        (self.get(files, id) && !doc.is_modified() && !self.warned.contains(&id))
            .then(|| (id, doc.revision()))
    }

    /// After a key: W10 when the watched buffer changed, or Insert or Replace mode began on it (nvim warns
    /// as Insert mode starts, before a character is typed).
    pub(crate) fn warn(
        &mut self,
        watched: Option<(DocumentId, Revision)>,
        ws: &Workspace,
        status: &mut Status,
    ) {
        let Some((id, rev)) = watched else { return };
        let changed = ws.document(id).is_some_and(|d| d.revision() != rev);
        let inserting = ws.focused_buffer() == id
            && matches!(
                ws.focused().view.mode(),
                Mode::Insert | Mode::Replace | Mode::VirtualReplace
            );
        if changed || inserting {
            self.warned.insert(id);
            status.warn(W10);
        }
    }
}

/// The per-buffer file registry: which buffers have a file on disk (a scratch `:enew` buffer has none, so
/// `:w` on it declines). Replaces the old single `path`/`fmt`/`file_buf` triple (F-007 multi-buffer). It also
/// carries how files are written ([`WriteOptions`], #619), so every write path reads one policy.
#[derive(Default)]
pub(crate) struct Files {
    map: HashMap<DocumentId, BufferFile>,
    /// The session's write options: `'backup'` and the rest, or the Emacs profile's variables.
    pub(crate) write: crate::app::write_opts::WriteOptions,
}

impl Files {
    pub(crate) fn new() -> Files {
        Files::default()
    }
}

impl std::ops::Deref for Files {
    type Target = HashMap<DocumentId, BufferFile>;
    fn deref(&self) -> &Self::Target {
        &self.map
    }
}

impl std::ops::DerefMut for Files {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.map
    }
}
