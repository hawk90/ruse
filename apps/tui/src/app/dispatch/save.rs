//! Write the focused buffer to its file (`:w`, a command-driven save).

use ruse_core::Workspace;

use super::{Files, E45};
use crate::app::messages::Status;
use crate::persist;

/// The bytes a write of `buffer` puts on disk for a file read as `fmt`: the final newline a buffer with
/// `'endofline'` (or `:set fixeol`) always gets (#1107), then the file's BOM and line endings (F-008 #2).
/// `ruse --replay` prints the same image of the replayed buffer, so it equals what `:w` wrote (#690).
pub(crate) fn disk_image(
    fmt: persist::encoding::FileFormat,
    buffer: &[u8],
    w: &crate::app::write_opts::WriteOptions,
) -> Vec<u8> {
    w.encode(buffer, fmt).disk
}

/// Returns whether the focused buffer reached disk.
pub(crate) fn save(ws: &mut Workspace, files: &Files, status: &mut Status) -> bool {
    // Multi-buffer honesty (F-007): only a buffer WITH a file writes. A scratch buffer (`:enew`) has no
    // registry entry, so `:w` declines rather than clobbering another buffer's file.
    let Some(bf) = files.get(&ws.focused_buffer()) else {
        status.error("E32: No file name");
        return false;
    };
    if bf.read_only {
        status.error(E45);
        return false;
    }
    if let Err(e) = files.write.check_write() {
        status.error(e);
        return false;
    }
    // Restore the original encoding/line-endings (F-008 #2) with the final newline the write options add,
    // then write durably (fsync + rename, #1) under the backup policy (#619).
    let enc = files.write.encode(ws.focused().doc.bytes(), bf.fmt);
    let bytes = &enc.disk;
    // A command-driven write is never forced: a read-only file is `E505`, as nvim's `ZZ` is, and a file
    // changed on disk since ruse read or saved it is not overwritten (RFC-0023 §8).
    if persist::disk::conflicts(&bf.path) {
        status.warn(persist::disk::CHANGED_SINCE_READ);
        return false;
    }
    match crate::app::write_opts::write_file(&files.write, &bf.path, bytes, false) {
        Ok(written) => {
            persist::disk::saw(&bf.path, bytes);
            ws.focused_doc_mut().mark_saved();
            persist::journal::clear(Some(bf.path.as_path())); // saved bytes are durable — nothing to recover
            crate::log::save(&bf.path, bytes.len());
            // Vim-style write report: `"file" 42L, 1024B written` (L = buffer lines, B = bytes on disk), with
            // `[noeol]` when the text written has no final newline, and what was not proven.
            status.echo(
                crate::app::file_cmds::write_report(
                    &bf.path.display().to_string(),
                    false,
                    &enc,
                    "written",
                    bf.fmt,
                ) + &written.note(),
            );
            true
        }
        Err(e) => {
            // Expected external failure (§7): surface it in the status bar, log once, keep the buffer.
            // Not saved: the buffer stays modified and keeps its journal (an fsync that failed after the
            // rename is `E667`, never "written", #936).
            crate::log::save_failed(&bf.path, &e);
            status.error(e.to_string());
            false
        }
    }
}
