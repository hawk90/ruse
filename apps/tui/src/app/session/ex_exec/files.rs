//! The file and buffer Ex commands (#1500): `:saveas`, `:bd`, the `:w` family, `:file`, `:cd`,
//! `:find`/`:sfind`, `:drop`, `:e`, and `:savetrace`. The ones that open a file return the `+cmd`
//! to run once the dispatch has let go of the session's fields.

use ruse_core::{Command, SplitDir};

use crate::app::dispatch::quit_window;
use crate::app::session::commands::info_status;
// `Trace` comes through the session's own imports; this module is its only user.
use crate::app::session::{Session, Trace};
use crate::input::{EditSpec, FileOpts, WriteSpec, WriteTarget};
use crate::persist;

impl Session {
    /// `:savetrace`.
    pub(in crate::app::session) fn ex_save_trace(&mut self, p: String) {
        let Session {
            ws,
            files,
            initial,
            start_options,
            start_registers,
            trace_watch,
            recorded,
            status,
            ..
        } = self;
        let trace =
            Trace::record_with_options(initial, start_options, start_registers, recorded.to_vec());
        // A trace that would not replay to this document is not written (#690). The trace holds
        // register contents and pasted clipboard text as well as the edits (RFC-0020 R6, Security
        // impact), so the message says so.
        let checked = crate::app::trace_check::check(&trace, initial, ws, files, trace_watch)
            .map_err(|r| r.to_string());
        match checked.and_then(|()| {
            std::fs::write(&p, trace.to_text()).map_err(|e| format!("trace save failed: {e}"))
        }) {
            Ok(()) => status.info(format!(
                "trace saved: {p} ({} commands; includes registers and clipboard text)",
                recorded.len()
            )),
            Err(e) => status.error(e),
        }
    }

    /// `:saveas`.
    pub(in crate::app::session) fn ex_saveas(&mut self, bang: bool, file: String, opts: FileOpts) {
        let Session {
            ws,
            files,
            highlighters,
            status,
            alt_file,
            ..
        } = self;
        let id = ws.focused_buffer();
        let old = files.get(&id).map(|bf| bf.path.display().to_string());
        let ok =
            crate::app::file_cmds::ex_saveas(bang, &file, opts, ws, files, highlighters, status);
        // The buffer's old name becomes the alternate file (Vim's 'cpoptions' `A`, #610).
        let renamed = files.get(&id).map(|bf| bf.path.display().to_string());
        if let Some(old) = old.filter(|o| ok && Some(o) != renamed.as_ref()) {
            *alt_file = Some(crate::app::ex_more::AltFile::new(ws, old));
        }
    }

    /// `:bd` deletes the focused buffer and drops its `files`/highlighter entries. Guards unsaved
    /// changes with E89 unless `!` forces it.
    pub(in crate::app::session) fn ex_bdelete(&mut self, force: bool) {
        let Session {
            ws,
            files,
            highlighters,
            status,
            ..
        } = self;
        let id = ws.focused_buffer();
        if !force && ws.focused().doc.is_modified() {
            status.error("E89: No write since last change (add ! to override)");
        } else {
            ws.remove_buffer(id);
            // Its changes go with it (`:bd!`), as nvim deletes the swap file (#862).
            if let Some(bf) = files.remove(&id) {
                persist::journal::discard(Some(bf.path.as_path()));
            }
            highlighters.remove(&id);
            status.info(format!("buffer {} deleted", id.0));
        }
    }

    /// The `:w` family (#610). `:wq`/`:x` close the window only after a successful write (#842): no
    /// file, or a failed write, keeps it open; `!` also lets the focused buffer's changes go.
    pub(in crate::app::session) fn ex_write(&mut self, spec: WriteSpec) {
        let Session {
            ws,
            files,
            highlighters,
            quit,
            status,
            alt_file,
            ..
        } = self;
        let id = ws.focused_buffer();
        let had_file = files.contains_key(&id);
        let ok = crate::app::file_cmds::ex_write(&spec, ws, files, highlighters, status);
        // Writing a buffer that has a file to another name makes that name the alternate file
        // (Vim's 'cpoptions' `A`, #610).
        if let (true, true, WriteTarget::File(Some(name))) = (ok, had_file, &spec.target) {
            let other = crate::app::dispatch::buffer_for_path(files, std::path::Path::new(name));
            if other != Some(id) {
                *alt_file = Some(crate::app::ex_more::AltFile::new(ws, name.clone()));
            }
        }
        if ok && spec.quit {
            quit_window(ws, status, quit, spec.bang);
        }
    }

    /// `:file` (#610): show the file information, rename the buffer, or remove its name. The old name
    /// becomes the alternate file.
    pub(in crate::app::session) fn ex_file(&mut self, cmd: crate::input::FileCmd) {
        let Session {
            ws,
            files,
            highlighters,
            status,
            alt_file,
            ..
        } = self;
        let id = ws.focused_buffer();
        let old = files.get(&id).map(|bf| bf.path.display().to_string());
        match cmd {
            crate::input::FileCmd::Info => {
                if let Some(msg) = info_status(&Command::FileInfo, ws, true) {
                    status.give(msg);
                }
            }
            // The name is expanded with the other file arguments above.
            crate::input::FileCmd::Rename(name) => {
                let fmt = files.get(&id).map(|bf| bf.fmt).unwrap_or_default();
                crate::app::file_cmds::adopt_file(ws, files, highlighters, id, &name, fmt);
                if let Some(old) = old.filter(|o| *o != name) {
                    *alt_file = Some(crate::app::ex_more::AltFile::new(ws, old));
                }
            }
            crate::input::FileCmd::Clear => {
                files.remove(&id);
                highlighters.remove(&id);
                ws.clear_focused_buffer_name();
                if let Some(old) = old {
                    *alt_file = Some(crate::app::ex_more::AltFile::new(ws, old));
                }
            }
        }
    }

    /// The directory is expanded with the other file arguments above.
    pub(in crate::app::session) fn ex_cd(&mut self, dir: Option<String>) {
        let Session {
            ws,
            files,
            status,
            prev_dir,
            ..
        } = self;
        match crate::app::ex_more::cd_target(dir.as_deref(), prev_dir.as_deref()) {
            Ok(target) => {
                let shown = dir.as_deref().unwrap_or("~");
                match crate::app::ex_more::change_dir(&target, shown, ws, files) {
                    Ok(old) => *prev_dir = Some(old),
                    Err(e) => status.error(e.to_string()),
                }
            }
            Err(e) => status.error(e.to_string()),
        }
    }

    /// `:find` / `:sfind` (#610): edit the file found in `'path'` (`.,,`), in a new window for `:sf`.
    pub(in crate::app::session) fn ex_find(
        &mut self,
        split: bool,
        spec: EditSpec,
    ) -> Option<String> {
        let Session {
            ws,
            files,
            highlighters,
            status,
            ..
        } = self;
        let mut then_run = None;
        let name = spec.file.clone().unwrap_or_default();
        let current = files.get(&ws.focused_buffer()).map(|bf| bf.path.clone());
        match crate::app::ex_more::find_in_path(&name, current.as_deref()) {
            Some(found) => {
                if split {
                    ws.split(SplitDir::Horizontal);
                }
                let spec = EditSpec {
                    file: Some(found),
                    ..spec
                };
                if crate::app::file_cmds::ex_edit(&spec, ws, files, highlighters, status) {
                    then_run = spec
                        .cmd
                        .as_deref()
                        .map(crate::app::file_cmds::plus_cmd_line);
                }
            }
            None => status.error(format!("E345: Can't find file \"{name}\" in path")),
        }
        then_run
    }

    /// `:drop {file}` (#610): go to a window that shows the file, or edit it here.
    pub(in crate::app::session) fn ex_drop(&mut self, spec: EditSpec) -> Option<String> {
        let Session {
            ws,
            files,
            highlighters,
            status,
            ..
        } = self;
        let mut then_run = None;
        let name = spec.file.clone().unwrap_or_default();
        let shown = crate::app::dispatch::buffer_for_path(files, std::path::Path::new(&name))
            .and_then(|id| (0..ws.window_count()).find(|&i| ws.pane(i).doc.id() == id));
        let opened = match shown {
            Some(i) => {
                ws.focus_window(i);
                true
            }
            None => crate::app::file_cmds::ex_edit(&spec, ws, files, highlighters, status),
        };
        if opened {
            then_run = spec
                .cmd
                .as_deref()
                .map(crate::app::file_cmds::plus_cmd_line);
        }
        then_run
    }

    /// `:e[!] [++ff] [+cmd] [{file}]` (F-007, #610): open a file into a buffer (shared with the file
    /// picker), or re-read the focused buffer's own file, which unsaved changes block (`E37`) without `!`.
    /// A `+cmd` runs once the file is open (after this dispatch, which holds the session's fields).
    pub(in crate::app::session) fn ex_edit(&mut self, spec: EditSpec) -> Option<String> {
        let Session {
            ws,
            files,
            highlighters,
            status,
            ..
        } = self;
        let mut then_run = None;
        if crate::app::file_cmds::ex_edit(&spec, ws, files, highlighters, status) {
            then_run = spec
                .cmd
                .as_deref()
                .map(crate::app::file_cmds::plus_cmd_line);
        }
        then_run
    }
}
