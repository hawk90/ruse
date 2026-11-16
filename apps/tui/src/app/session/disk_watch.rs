//! The recovery journals, the look at the files on disk (RFC-0023 §8) and `autosave`: their syncs and
//! their wake-ups. Split from `session.rs` (#1423).

use super::*;

impl Session {
    /// Remove the journal of every buffer this session could have journaled (#862), on a normal quit: each
    /// writer's (also one beside a live other session's, #1193), each file buffer's, and the unnamed start
    /// buffer's. Only journals this process owns go ([`persist::journal::Journals::discard`]).
    pub(crate) fn discard_journals(&mut self) {
        for w in self.journal_writers.values_mut() {
            w.discard();
        }
        let Some(j) = self.journals else {
            return;
        };
        for bf in self.files.values() {
            j.discard(Some(bf.path.as_path()));
        }
        if self.unnamed_start.is_some() {
            j.discard(None);
        }
    }

    /// Sync every buffer's pending journal records now: before a risky operation (`:!`, a filter, `:r !`,
    /// `:w !`, `:terminal`, an LSP edit batch), so a hang or a kill inside it loses nothing that was typed
    /// (RFC-0023 §4).
    pub(crate) fn sync_journals(&mut self) {
        let Session {
            ws,
            files,
            journal_writers,
            journal_failed,
            status,
            ..
        } = self;
        sync_journals(ws, files, journal_writers, journal_failed, status);
    }

    /// How long until a buffered journal sync is due, for the event loop's poll timeout.
    pub(super) fn journal_wake_ms(&self) -> Option<u64> {
        let now = std::time::Instant::now();
        self.journal_writers
            .values()
            .filter_map(|w| w.wake_in(now))
            .min()
            .map(|d| (d.as_millis() as u64).max(1))
    }

    /// Look at every file buffer's file again (RFC-0023 §8): a stat, and a hash when it moved. A buffer
    /// whose file newly differs from what ruse read or saved gets nvim's W11 (unmodified) or W12 (modified
    /// too) on the status line when `announce`, the focused buffer first; one whose file is gone gets
    /// nvim's E211 and no `[disk changed]` flag. With `'autoread'` (on, as nvim) an unmodified buffer whose
    /// file changed is re-read instead, silently and as one undoable change (#619); a modified one still
    /// gets W12 and keeps its text. ruse does not offer to reload.
    pub(crate) fn check_disk(&mut self, announce: bool) {
        self.disk_checked = std::time::Instant::now();
        // The config layer files too (#1152); a file's own warning, below, is shown over its report.
        self.watch_config();
        let focused = self.ws.focused_buffer();
        let mut ids: Vec<DocumentId> = self.files.keys().copied().collect();
        ids.sort_by_key(|id| (*id != focused, id.0));
        let mut first = None;
        for id in ids {
            let Some(bf) = self.files.get(&id) else {
                continue;
            };
            let mut state = persist::disk::check(&bf.path);
            let clean = self.ws.document(id).is_some_and(|d| !d.is_modified());
            if state == persist::disk::State::Changed
                && clean
                && self.ui_opts.buffers.autoread
                && self.autoread(id)
            {
                state = persist::disk::State::Same;
            }
            if state == persist::disk::State::Changed {
                self.disk_changed.insert(id);
            } else {
                self.disk_changed.remove(&id);
            }
            if state == persist::disk::State::Same {
                self.disk_warned.remove(&id);
            } else if self.disk_warned.get(&id) != Some(&state) && first.is_none() {
                first = Some((id, state));
            }
        }
        self.disk_changed.retain(|id| self.files.contains_key(id));
        self.disk_warned.retain(|id, _| self.files.contains_key(id));
        // Emacs says nothing until the buffer is visited or edited again (find-file, #619): the W11/W12/E211
        // messages are nvim's.
        let Some((id, state)) = first.filter(|_| announce && !self.emacs_profile) else {
            return;
        };
        self.disk_warned.insert(id, state);
        let (Some(bf), Some(doc)) = (self.files.get(&id), self.ws.document(id)) else {
            return;
        };
        let name = bf.path.display();
        if state == persist::disk::State::Gone {
            self.status
                .error(format!("E211: File \"{name}\" no longer available"));
        } else if doc.is_modified() {
            self.status.warn(format!(
                "W12: Warning: File \"{name}\" has changed and the buffer was changed in Vim as well"
            ));
        } else {
            self.status.warn(format!(
                "W11: Warning: File \"{name}\" has changed since editing started"
            ));
        }
    }

    /// `'autoread'`: re-read the unmodified buffer `id` from its changed file as one undoable change, with
    /// the file's new line endings and BOM. Returns whether it did (an unreadable file is left to the
    /// warning).
    pub(super) fn autoread(&mut self, id: DocumentId) -> bool {
        let Some(path) = self.files.get(&id).map(|bf| bf.path.clone()) else {
            return false;
        };
        let Ok(raw) = persist::read_file(&path) else {
            return false;
        };
        persist::disk::saw(&path, &raw);
        let fmt = self.files.write.read_format(&raw);
        self.ws.reload_undoable(id, fmt.to_buffer(&raw));
        if let Some(bf) = self.files.get_mut(&id) {
            bf.fmt = fmt;
            bf.reloads = bf.reloads.saturating_add(1);
        }
        persist::journal::clear(Some(&path));
        true
    }

    /// The idle trigger of [`Session::check_disk`]: once [`persist::journal::IDLE`] passed since the last
    /// key and the last check. Only a session with files on disk to watch (the binary's) runs it.
    pub(crate) fn idle_disk_check(&mut self) {
        let idle = persist::journal::IDLE;
        if self.rescue
            && (!self.files.is_empty() || self.config_paths.is_some())
            && self.last_input.elapsed() >= idle
            && self.disk_checked.elapsed() >= idle
        {
            self.check_disk(true);
        }
    }

    /// Emacs's `auto-save-visited-mode` (#619): once `auto-save-visited-interval` seconds passed since the
    /// last key, save every modified buffer that visits a file (`save-some-buffers` without asking), once per
    /// idle period, as Emacs's repeating idle timer does. Not a read-only buffer, which Emacs cannot save.
    pub(crate) fn idle_auto_save_visited(&mut self) {
        let Some(interval) = self.auto_save_interval() else {
            return;
        };
        if self.last_input.elapsed() < interval || self.auto_saved_since_input {
            return;
        }
        self.auto_saved_since_input = true;
        let skip: Vec<DocumentId> = self
            .files
            .iter()
            .filter(|(id, bf)| {
                bf.read_only || self.ws.document(**id).is_some_and(|d| !d.is_modifiable())
            })
            .map(|(id, _)| *id)
            .collect();
        let Session {
            ws, files, status, ..
        } = self;
        if ws
            .buffers()
            .iter()
            .any(|b| b.modified && files.contains_key(&b.id) && !skip.contains(&b.id))
        {
            crate::app::file_cmds::ex_write_all(ws, files, status, |b| skip.contains(&b));
        }
    }

    /// `auto-save-visited-interval` while `auto-save-visited-mode` is on (Emacs profile), else `None`.
    pub(super) fn auto_save_interval(&self) -> Option<std::time::Duration> {
        let e = self.files.write.emacs.as_ref()?;
        (e.auto_save_visited_mode && self.rescue)
            .then(|| std::time::Duration::from_secs(e.auto_save_visited_interval.max(1)))
    }

    /// How long until [`Session::idle_auto_save_visited`] is due, for the poll timeout.
    pub(super) fn auto_save_wake_ms(&self) -> Option<u64> {
        let interval = self.auto_save_interval()?;
        if self.auto_saved_since_input {
            return None;
        }
        let wait = interval.saturating_sub(self.last_input.elapsed());
        Some((wait.as_millis() as u64).max(1))
    }

    /// How long until [`Session::idle_disk_check`] is due, for the poll timeout.
    pub(super) fn disk_wake_ms(&self) -> Option<u64> {
        if !self.rescue || (self.files.is_empty() && self.config_paths.is_none()) {
            return None;
        }
        let idle = persist::journal::IDLE;
        let wait = idle
            .saturating_sub(self.last_input.elapsed())
            .max(idle.saturating_sub(self.disk_checked.elapsed()));
        Some((wait.as_millis() as u64).max(1))
    }
}

/// The journal part of `:checkhealth` (RFC-0023 §8): the journal directory and each file buffer's journal,
/// in buffer order.
pub(super) fn journal_health(
    rescue: bool,
    journals: Option<&persist::journal::Journals>,
    files: &Files,
    ws: &Workspace,
    writers: &HashMap<DocumentId, persist::journal::Writer>,
) -> health::Journaling {
    if !rescue {
        return health::Journaling::Off;
    }
    let Some(j) = journals else {
        return health::Journaling::NoStateDir;
    };
    let now = std::time::Instant::now();
    let mut ids: Vec<DocumentId> = files.keys().copied().collect();
    ids.sort_by_key(|id| id.0);
    let buffers = ids
        .into_iter()
        .filter_map(|id| {
            let bf = files.get(&id)?;
            Some(health::JournalInput {
                file: bf.path.display().to_string(),
                modified: ws.document(id).is_some_and(|d| d.is_modified()),
                health: writers.get(&id).map(|w| w.health(now)),
            })
        })
        .collect();
    health::Journaling::On {
        dir: j.dir().display().to_string(),
        archive: persist::retention::stats(j),
        buffers,
    }
}

/// [`Session::sync_journals`] over the session's fields, for a caller that holds them borrowed apart.
pub(super) fn sync_journals(
    ws: &mut Workspace,
    files: &Files,
    journal_writers: &mut HashMap<DocumentId, persist::journal::Writer>,
    journal_failed: &mut HashSet<DocumentId>,
    status: &mut Status,
) {
    let now = std::time::Instant::now();
    for (id, w) in journal_writers.iter_mut() {
        let (Some(doc), Some(bf)) = (ws.document_mut(*id), files.get(id)) else {
            continue;
        };
        w.absorb(doc, bf.fmt, now); // the changes made since the frame began, too
        if w.unsynced() {
            let r = w.flush(doc);
            journal_outcome(r, *id, &bf.path, w.journal(), journal_failed, status);
        }
    }
}

/// Report a journal sync: a failure is shown once per failing run, as Neovim's E303 is, rather than
/// silently leaving the work unprotected (#907); the records stay buffered for the next try.
/// The status line then shows `[journal!]` until a write succeeds (RFC-0023 §8).
pub(super) fn journal_outcome(
    r: io::Result<()>,
    id: DocumentId,
    path: &std::path::Path,
    journal: &std::path::Path,
    failed: &mut HashSet<DocumentId>,
    status: &mut Status,
) {
    match r {
        Ok(()) => {
            failed.remove(&id);
        }
        Err(e) => {
            if failed.insert(id) {
                crate::log::journal_failed(path, &e);
                status.error(format!(
                    "E303: unable to write the recovery journal for \"{}\": {e} ({})",
                    path.display(),
                    journal.display()
                ));
            }
        }
    }
}
