//! Open a file into a buffer: read it, offer a crash journal's recovery, and attach its file identity
//! and highlighter.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use ruse_core::{BytePos, Command, DocumentId, ExCmd, Workspace};

use super::{note_ex, run_ex, BufferFile, Files, Highlighters};
use crate::app::messages::Msg;
use crate::highlight;
use crate::persist;

/// What [`open_file_into_buffer`] did: the status line to show, and the buffer now focused on `file`
/// (`None` when nothing was opened — a read error, or the user kept a recovery journal and backed out).
pub(crate) struct Opened {
    pub(crate) status: crate::app::messages::Msg,
    pub(crate) id: Option<DocumentId>,
}

/// The buffer already holding `path`, matched on the canonical path so `f.txt`, `./f.txt` and an
/// absolute spelling name one file (#848). A path that no longer resolves compares as written.
pub(crate) fn buffer_for_path(files: &Files, path: &Path) -> Option<DocumentId> {
    let canon = |p: &Path| fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let target = canon(path);
    files
        .iter()
        .filter(|(_, bf)| canon(&bf.path) == target)
        .map(|(id, _)| *id)
        .min_by_key(|id| id.0)
}

/// Open `file` and focus it, register its file identity + highlighter, and return a status line. Shared
/// by the file picker / `:e` (session) and goto/rename/references (LSP coordinator). `E484` on a read
/// error.
///
/// A file that is already open is NOT read again: its existing buffer is focused, as in Neovim (#848),
/// so two Documents never shadow one file and overwrite each other on `:w`. A file with a crash journal
/// that differs from disk gets the same recovery prompt as the startup file (#859); keeping the journal
/// (`q`) cancels the open, so the journal is never cleared by a later `:w` the user did not choose.
pub(crate) fn open_file_into_buffer(
    file: &str,
    ws: &mut Workspace,
    files: &mut Files,
    highlighters: &mut Highlighters,
) -> Opened {
    open_file_with(file, ws, files, highlighters, |fmt| fmt)
}

/// [`open_file_into_buffer`], with `format` adjusting the detected file format (`:e ++ff=dos`). A file
/// that does not exist yet opens as an empty buffer that `:w` creates (`"new.txt" [New]`, as in nvim).
pub(crate) fn open_file_with(
    file: &str,
    ws: &mut Workspace,
    files: &mut Files,
    highlighters: &mut Highlighters,
    format: impl FnOnce(persist::encoding::FileFormat) -> persist::encoding::FileFormat,
) -> Opened {
    let p = PathBuf::from(file);
    if let Some(id) = buffer_for_path(files, &p) {
        ws.focus_buffer(id);
        return Opened {
            status: Msg::echo(format!("\"{file}\" (already open)")),
            id: Some(id),
        };
    }
    let (raw, new) = match persist::read_file(&p) {
        Ok(raw) => (raw, false),
        // A dangling symlink is not a new file: writing it would create the link's target.
        Err(e) if e.kind() == io::ErrorKind::NotFound && fs::symlink_metadata(&p).is_err() => {
            (Vec::new(), true)
        }
        Err(e) => {
            crate::log::open_failed("edit", &p, &e);
            return Opened {
                status: Msg::error(format!("E484: can't open {file}: {e}")),
                id: None,
            };
        }
    };
    // A forced format (`++ff=`, `++bin`) reads the final newline anew (RFC-0024).
    let mut f = format(files.write.read_format(&raw)).reading(&raw);
    let disk = f.to_buffer(&raw);
    // The same prompt as at startup (RFC-0023 §7); `q` cancels this open and keeps the journal.
    let offer =
        persist::journal::current().map_or(persist::Recovery::None, |j| j.offer(Some(&p), &disk));
    let resolved =
        crate::ui::recovery::resolve(&mut io::stdout(), offer, file, Some(&p), &disk, f, "cancel");
    let (recovered, read_only, note) = match resolved {
        Ok(crate::ui::recovery::Resolved::Open {
            recovered,
            read_only,
            note,
            format,
        }) => {
            // A recovered buffer takes the format its journal recorded (#1286).
            if let Some(format) = format {
                f = format;
            }
            (recovered, read_only, note)
        }
        Ok(crate::ui::recovery::Resolved::Quit { journal }) => {
            return Opened {
                status: Msg::info(format!(
                    "\"{file}\" not opened: recovery journal kept ({})",
                    journal.display()
                )),
                id: None,
            }
        }
        Err(e) => {
            return Opened {
                status: Msg::error(format!(
                    "\"{file}\" not opened: the recovery prompt failed: {e}"
                )),
                id: None,
            }
        }
    };
    let id = ws.add_buffer(disk, Some(file.to_string()));
    ws.focus_buffer(id);
    if let Some(rec) = recovered {
        // A buffer opened later is not part of a trace (it starts from the startup buffer).
        apply_recovery(ws, rec, &mut Vec::new());
        ws.place_focused_cursor(BytePos(0));
    }
    // A file message: shown, not kept (nvim's `:e` message is not in `:messages`).
    let status = Msg::echo(match note {
        Some(note) => format!("\"{file}\" {note}"),
        None if new => format!("\"{file}\" [New]"),
        None => format!("\"{file}\" {} bytes", raw.len()),
    });
    persist::disk::saw(&p, &raw);
    attach_file(files, highlighters, id, p, f);
    mark_large(files, highlighters, id, raw.len());
    if let Some(bf) = files.get_mut(&id) {
        bf.read_only = read_only;
    }
    Opened {
        status,
        id: Some(id),
    }
}

/// Put a recovered text over the focused buffer's disk text (#863, #1194) so the buffer is modified (`:q`
/// refuses with E37) and `u` goes back to the disk text: the commands [`persist::recovery_commands`] gives
/// (one text edit, or a filter or read of the bytes plus a final-newline edit), run through [`run_ex`] into
/// `recorded` as one undo step. Two of them are an `ex_begin`/`ex_end` group, so a saved trace replays them
/// as one step too. The group is labelled `macro` (keys that were not typed: one undo step); the trace has
/// no kind of its own for a recovery, and adding one would change its format.
pub(crate) fn apply_recovery(ws: &mut Workspace, rec: Vec<u8>, recorded: &mut Vec<Command>) {
    // Buffer text is line-terminated (RFC-0024): the offer gives it so, and so does the buffer.
    let rec = persist::encoding::terminated(rec);
    // Vim's line model has the emptied buffer's line; the Emacs profile's empty buffer is no text.
    let vim = ws.focused().view.caret_gravity() == ruse_core::CaretGravity::OnChar;
    let cmds = persist::recovery_commands(ws.focused().doc.bytes(), &rec, vim);
    let group = cmds.len() > 1;
    let opened = group && !ws.focused().doc.in_undo_block();
    if opened {
        ws.focused_doc_mut().begin_undo_block();
    }
    if group {
        note_ex(ExCmd::Begin(ruse_core::GroupKind::Macro), recorded);
    }
    for ex in cmds {
        run_ex(ex, ws, recorded);
    }
    if group {
        note_ex(ExCmd::End, recorded);
    }
    if opened {
        ws.focused_doc_mut().end_undo_block();
    }
    debug_assert_eq!(ws.focused().doc.bytes(), &rec[..]);
}

/// Give buffer `id` its file on disk: record `path` and `fmt` in the file registry and attach the
/// highlighter `path`'s extension selects, dropping any the buffer had (#894). Every way a buffer gets a
/// file goes through here: the startup file, `:e`/the file picker/LSP jumps, and `:w {file}`/`:saveas`.
pub(crate) fn attach_file(
    files: &mut Files,
    highlighters: &mut Highlighters,
    id: DocumentId,
    path: PathBuf,
    fmt: persist::encoding::FileFormat,
) {
    // Emacs's `auto-mode-case-fold` (#619): an extension no mode knows is tried again in lower case.
    let fold = files
        .write
        .emacs
        .as_ref()
        .is_some_and(|e| e.auto_mode_case_fold);
    let ext = path.extension().and_then(|e| e.to_str());
    match ext
        .and_then(highlight::CachedHighlight::for_ext)
        .or_else(|| {
            ext.filter(|_| fold)
                .and_then(|e| highlight::CachedHighlight::for_ext(&e.to_ascii_lowercase()))
        }) {
        Some(h) => {
            highlighters.insert(id, h);
        }
        None => {
            highlighters.remove(&id);
        }
    }
    persist::backup::visited(&path); // a new visit: Emacs makes its one backup again
    let swapfile = files.write.swapfile;
    files.insert(
        id,
        BufferFile {
            path,
            fmt,
            read_only: false,
            reloads: 0,
            large: false,
            swapfile,
        },
    );
}

/// The degraded large-file profile (#619, RFC-0008 §7, COM-12): a file read past the large-file threshold
/// ([`persist::rj01::LARGE_FILE`], 32 MiB) gets no syntax highlighting, and the status line says
/// `[large]`.
pub(crate) fn mark_large(
    files: &mut Files,
    highlighters: &mut Highlighters,
    id: DocumentId,
    len: usize,
) {
    if len > persist::rj01::LARGE_FILE {
        highlighters.remove(&id);
        if let Some(bf) = files.get_mut(&id) {
            bf.large = true;
        }
    }
}
