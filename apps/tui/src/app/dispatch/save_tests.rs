use std::path::PathBuf;

use super::*;
use crate::persist;

/// nvim v0.12.6's write report tags: `[New][noeol][dos]` after one blank, none for a unix file or a
/// binary write.
#[test]
fn write_report_tags_follow_nvim() {
    use crate::app::file_cmds::write_report;
    use crate::app::write_opts::Encoded;
    use persist::encoding::{Eol, FileFormat};
    let unix = FileFormat::default();
    let dos = FileFormat {
        eol: Eol::Crlf,
        ..unix
    };
    let enc = |source: &[u8], disk: usize, noeol| Encoded {
        source: source.to_vec(),
        disk: vec![0; disk],
        noeol,
    };
    assert_eq!(
        write_report("f", false, &enc(b"a\n", 2, false), "written", unix),
        "\"f\" 1L, 2B written"
    );
    assert_eq!(
        write_report("f", false, &enc(b"a\n", 1, true), "written", unix),
        "\"f\" [noeol] 1L, 1B written"
    );
    // RFC-0024: one empty line without 'endofline' is a line and zero bytes; the emptied buffer is none.
    assert_eq!(
        write_report("f", false, &enc(b"\n", 0, true), "written", unix),
        "\"f\" [noeol] 1L, 0B written"
    );
    assert_eq!(
        write_report("f", false, &enc(b"", 0, false), "written", unix),
        "\"f\" 0L, 0B written"
    );
    assert_eq!(
        write_report("f", true, &enc(b"a\n", 3, false), "written", dos),
        "\"f\" [New][dos] 1L, 3B written"
    );
    assert_eq!(
        write_report(
            "f",
            false,
            &enc(b"a\n", 1, true),
            "written",
            FileFormat {
                eol: Eol::Cr,
                ..unix
            }
        ),
        "\"f\" [noeol][mac] 1L, 1B written"
    );
    let bin = FileFormat {
        binary: true,
        ..dos
    };
    assert_eq!(
        write_report("f", false, &enc(b"a\n", 2, false), "written", bin),
        "\"f\" 1L, 2B written"
    );
}

/// A workspace whose focused buffer is the (new) file `name` in a fresh temp dir, edited once.
fn ws_on_temp_file(name: &str, text: &[u8]) -> (Workspace, Files, PathBuf) {
    use std::sync::atomic::{AtomicU32, Ordering};
    static N: AtomicU32 = AtomicU32::new(0);
    let dir = std::env::temp_dir().join(format!(
        "ruse-dispatch-save-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    // The file outlives this helper, so no `TestDir` guard: a dir left by an earlier run of the
    // same pid is emptied instead (#1366).
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(name);
    std::fs::write(&path, text).unwrap();
    let mut ws = Workspace::new(text.to_vec());
    ws.apply(&Command::EnterInsert);
    ws.apply(&Command::InsertChar('x'));
    ws.apply(&Command::EnterNormal);
    let (mut files, mut hl): (Files, Highlighters) = (Files::new(), HashMap::new());
    let fmt = persist::encoding::FileFormat::detect(text);
    attach_file(&mut files, &mut hl, ws.focused_buffer(), path.clone(), fmt);
    (ws, files, path)
}

/// #936: when the directory fsync fails after the rename, `:w` does not report "written" and the buffer
/// stays modified (it was "written" and clean before).
#[cfg(unix)]
#[test]
fn save_with_a_failed_directory_fsync_is_not_saved() {
    let (mut ws, files, path) = ws_on_temp_file("f.txt", b"a\n");
    let mut status = crate::app::messages::Status::default();
    persist::atomic::inject(Some(persist::atomic::Step::SyncDir));
    let ok = save(&mut ws, &files, &mut status);
    persist::atomic::inject(None);
    assert!(!ok);
    assert!(status.starts_with("E667: Fsync failed"), "{status}");
    assert!(!status.ends_with("B written"), "{status}");
    assert!(ws.focused().doc.is_modified(), "not saved: still modified");
    assert_eq!(std::fs::read(&path).unwrap(), b"xa\n");
    // A later save that succeeds clears it.
    assert!(save(&mut ws, &files, &mut status));
    assert!(status.ends_with("written"), "{status}");
    assert!(!ws.focused().doc.is_modified());
}

/// A filesystem that does not support a directory fsync (EINVAL) saves: the buffer is clean and the
/// journal cleared, and the report says the rename's durability is not proven instead of failing.
#[cfg(unix)]
#[test]
fn save_where_directory_fsync_is_unsupported_is_saved_with_a_note() {
    let (mut ws, files, path) = ws_on_temp_file("f.txt", b"a\n");
    let mut status = crate::app::messages::Status::default();
    persist::atomic::inject_errno(persist::atomic::Step::SyncDir, libc::EINVAL);
    let ok = save(&mut ws, &files, &mut status);
    persist::atomic::inject(None);
    assert!(ok, "{status}");
    assert_eq!(
        status,
        format!(
            "\"{}\" 1L, 3B written (directory sync not supported here; rename durability not proven)",
            path.display()
        )
    );
    assert!(!ws.focused().doc.is_modified());
    assert_eq!(std::fs::read(&path).unwrap(), b"xa\n");
}

/// #872: `:set fixeol` adds the newline to the written file only, the buffer stays as typed and is clean,
/// as in nvim v0.12.6 (`'eol'` stays off, `&modified` 0; a reload then reads the newline). ruse's buffer
/// bytes play nvim's lines + `'eol'`, so a later `:set nofixeol` write omits the newline again, as nvim's.
#[test]
fn fixeol_writes_the_newline_without_changing_the_buffer() {
    let (mut ws, mut files, path) = ws_on_temp_file("n.txt", b"a");
    let mut status = crate::app::messages::Status::default();
    files.write.fixeol = true;
    assert!(save(&mut ws, &files, &mut status));
    assert_eq!(status, format!("\"{}\" 1L, 3B written", path.display()));
    assert_eq!(std::fs::read(&path).unwrap(), b"xa\n");
    assert_eq!(ws.focused().doc.bytes(), b"xa", "the buffer is not edited");
    assert!(!ws.focused().doc.is_modified(), "and is clean");
    ws.apply(&Command::EnterInsert);
    ws.apply(&Command::InsertChar('y'));
    ws.apply(&Command::EnterNormal);
    files.write.fixeol = false;
    assert!(save(&mut ws, &files, &mut status));
    assert_eq!(
        std::fs::read(&path).unwrap(),
        b"yxa",
        "nofixeol: no newline"
    );
    assert!(status.contains("[noeol]"), "{status}");
}
