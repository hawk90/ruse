//! Durable persistence & crash recovery (F-008). MVP-minimal, honest against all four acceptance
//! criteria; the full four-state / inverse-edit / incremental journal in
//! `docs/design/persistence-and-recovery.md` is the post-MVP C-PERSIST elaboration.
//!
//!   * [`atomic`]   — temp + fsync + rename + directory fsync (durable "Saved"). (#1)
//!   * [`encoding`] — detect the original EOL/BOM and restore it on save. (#2)
//!   * [`journal`]  — append-only framed recovery records; a torn tail is discarded. (#4)
//!   * [`assess_recovery`] — the pure open-time decision; recovery never auto-overwrites. (#3, #4)
//!
//! The pure cores are terminal-free and file-free (they take/return bytes) so the whole persistence
//! contract is unit-testable; only the thin `append`/`replay`/`save` wrappers touch the disk.

pub mod atomic;
pub mod encoding;
pub mod journal;

/// What a document's journal offers at open time, once compared against the on-disk file.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Recovery {
    /// No journal, or its latest record already matches the disk file — nothing to recover.
    None,
    /// Unsaved changes were found that differ from disk. The user chooses: keep the recovered
    /// buffer, or discard it and use the disk file. The original file is NEVER auto-overwritten —
    /// recovered bytes go into the editor buffer only, so a save is an explicit later act.
    Available(Vec<u8>),
}

/// The open-time recovery decision (F-008 #3/#4), pure over the two byte sources so it is testable
/// without a filesystem. `disk` is the file's current content (the editor's cold-open buffer, after
/// [`encoding::FileFormat::to_buffer`]); `recovered` is [`journal::replay`]'s latest valid payload.
///
/// A recovery is offered only when the journal holds work that genuinely differs from disk — a stale
/// journal left after a clean exit (its last record == disk) yields `None`, so the user is not
/// nagged about nothing.
pub fn assess_recovery(disk: &[u8], recovered: Option<&[u8]>) -> Recovery {
    match recovered {
        Some(r) if r != disk => Recovery::Available(r.to_vec()),
        _ => Recovery::None,
    }
}

/// The recovered text as one edit over the disk text (#863): `(start, end, text)` replaces `disk[start..end]`
/// with `text` and yields `recovered`. Only the span that differs is replaced, so the recovery is a small
/// change the trace records and `u` takes back to the disk text. The span is widened to character
/// boundaries; `None` when its recovered side is not valid UTF-8, which a text edit cannot carry, or when
/// no boundary exists to cut the disk text at (it opens with a stray continuation byte).
pub fn recovery_edit(disk: &[u8], recovered: &[u8]) -> Option<(usize, usize, String)> {
    let cont = |b: &[u8], i: usize| i < b.len() && (b[i] & 0xC0) == 0x80;
    let mut pre = disk
        .iter()
        .zip(recovered)
        .take_while(|(a, b)| a == b)
        .count();
    while pre > 0 && (cont(disk, pre) || cont(recovered, pre)) {
        pre -= 1;
    }
    if cont(disk, pre) {
        return None; // a stray continuation byte opens the disk text: no boundary to cut at
    }
    let room = disk.len().min(recovered.len()) - pre;
    let mut suf = disk
        .iter()
        .rev()
        .zip(recovered.iter().rev())
        .take(room)
        .take_while(|(a, b)| a == b)
        .count();
    while suf > 0 && (cont(disk, disk.len() - suf) || cont(recovered, recovered.len() - suf)) {
        suf -= 1;
    }
    let text = String::from_utf8(recovered[pre..recovered.len() - suf].to_vec()).ok()?;
    Some((pre, disk.len() - suf, text))
}

/// Read a file to open it as a buffer, refusing paths that are not files instead of hanging or reading
/// without end (#907). The type is checked (symlinks followed) before anything is read:
///
/// * a directory is an error (`is a directory`);
/// * a character or block device or a socket is an error (`is not a file`, Neovim's message) — `/dev/zero`
///   would otherwise be read until memory runs out;
/// * a FIFO is opened without blocking. With no writer it is an error (`is a FIFO with no writer`) where
///   Neovim v0.12.6 hangs before any UI; with a writer (`ruse <(cmd)`) it is read to EOF as usual.
///
/// A missing file keeps its `NotFound` error so callers can still open it as a new buffer.
pub fn read_file(path: &std::path::Path) -> std::io::Result<Vec<u8>> {
    use std::io::{Error, ErrorKind};
    let meta = std::fs::metadata(path)?;
    let ft = meta.file_type();
    if ft.is_dir() {
        return Err(Error::new(ErrorKind::IsADirectory, "is a directory"));
    }
    if ft.is_file() {
        return std::fs::read(path);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileTypeExt;
        if ft.is_fifo() {
            return read_fifo(path);
        }
    }
    Err(Error::new(ErrorKind::InvalidInput, "is not a file"))
}

/// Read a FIFO without blocking on its open: `O_NONBLOCK` makes the open return at once, a first read of
/// EOF means no writer is connected (an error), and otherwise blocking mode is restored and the stream is
/// read to EOF.
#[cfg(unix)]
fn read_fifo(path: &std::path::Path) -> std::io::Result<Vec<u8>> {
    use std::io::{Error, ErrorKind, Read};
    use std::os::unix::fs::OpenOptionsExt;
    use std::os::unix::io::AsRawFd;
    let mut f = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)?;
    let mut out = Vec::new();
    let mut buf = [0u8; 8192];
    match f.read(&mut buf) {
        Ok(0) => {
            return Err(Error::new(
                ErrorKind::WouldBlock,
                "is a FIFO with no writer",
            ))
        }
        Ok(n) => out.extend_from_slice(&buf[..n]),
        Err(e) if e.kind() == ErrorKind::WouldBlock => {} // a writer is connected; no data yet
        Err(e) => return Err(e),
    }
    let fd = f.as_raw_fd();
    // SAFETY: `fd` is a valid open descriptor owned by `f` for the duration of both calls.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags & !libc::O_NONBLOCK) } < 0 {
        return Err(Error::last_os_error());
    }
    f.read_to_end(&mut out)?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_file_refuses_directories_and_devices() {
        let dir = std::env::temp_dir().join(format!("ruse-read-file-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let e = read_file(&dir).unwrap_err();
        assert_eq!(e.to_string(), "is a directory");
        #[cfg(unix)]
        {
            let e = read_file(std::path::Path::new("/dev/zero")).unwrap_err();
            assert_eq!(e.to_string(), "is not a file");
        }
        let f = dir.join("f.txt");
        std::fs::write(&f, b"text\n").unwrap();
        assert_eq!(read_file(&f).unwrap(), b"text\n");
        let missing = read_file(&dir.join("missing")).unwrap_err();
        assert_eq!(missing.kind(), std::io::ErrorKind::NotFound);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A FIFO with no writer is refused at once instead of blocking the open; one with a writer is read.
    #[cfg(unix)]
    #[test]
    fn read_file_does_not_block_on_a_fifo() {
        use std::os::unix::ffi::OsStrExt;
        let dir = std::env::temp_dir().join(format!("ruse-read-fifo-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let fifo = dir.join("p");
        let c = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
        // SAFETY: `c` is a valid NUL-terminated path.
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        let t = std::time::Instant::now();
        let e = read_file(&fifo).unwrap_err();
        assert_eq!(e.to_string(), "is a FIFO with no writer");
        assert!(t.elapsed() < std::time::Duration::from_secs(2));

        // A writer holding the FIFO open (as `<(cmd)` does) is read through to EOF.
        // A placeholder reader lets the write-open return at once; the bytes wait in the pipe buffer.
        let placeholder = {
            use std::os::unix::fs::OpenOptionsExt;
            std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(&fifo)
                .unwrap()
        };
        let mut w = std::fs::OpenOptions::new().write(true).open(&fifo).unwrap();
        std::io::Write::write_all(&mut w, b"piped\n").unwrap();
        let writer = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(50));
            drop(w); // EOF for the reader
        });
        assert_eq!(read_file(&fifo).unwrap(), b"piped\n");
        writer.join().unwrap();
        drop(placeholder);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn apply(disk: &[u8], (s, e, t): &(usize, usize, String)) -> Vec<u8> {
        [&disk[..*s], t.as_bytes(), &disk[*e..]].concat()
    }

    #[test]
    fn recovery_edit_replaces_only_the_changed_span() {
        let e = recovery_edit(b"one\ntwo\nthree\n", b"one\nTWO!\nthree\n").unwrap();
        assert_eq!(e, (4, 7, "TWO!".into()));
        assert_eq!(
            recovery_edit(b"", b"new\n").unwrap(),
            (0, 0, "new\n".into())
        );
        assert_eq!(
            recovery_edit(b"gone\n", b"").unwrap(),
            (0, 5, String::new())
        );
        // A shared lead byte of two different characters is not split.
        let e = recovery_edit("é".as_bytes(), "è".as_bytes()).unwrap();
        assert_eq!(e, (0, 2, "è".into()));
        // Unchanged non-UTF-8 bytes around the change are fine; a non-UTF-8 change is not a text edit.
        let e = recovery_edit(b"caf\xe9 x\n", b"caf\xe9 y\n").unwrap();
        assert_eq!(apply(b"caf\xe9 x\n", &e), b"caf\xe9 y\n");
        assert_eq!(recovery_edit(b"x\n", b"\xff\n"), None);
    }

    mod fuzz {
        use super::*;
        use proptest::prelude::*;

        proptest! {
            #![proptest_config(crate::test_cases(256))]

            /// Whenever an edit is returned, it turns the disk text into the recovered text exactly, and
            /// its cut points are character boundaries of the disk text (what `apply_edits` requires).
            #[test]
            fn recovery_edit_reproduces_the_recovered_text(
                disk in proptest::collection::vec(prop_oneof![Just(b'a'), Just(b'\n'), Just(0xC3u8), Just(0xA9u8), Just(0xFFu8)], 0..24),
                rec in proptest::collection::vec(prop_oneof![Just(b'a'), Just(b'b'), Just(b'\n'), Just(0xC3u8), Just(0xA9u8)], 0..24),
            ) {
                if let Some(e) = recovery_edit(&disk, &rec) {
                    prop_assert_eq!(apply(&disk, &e), rec);
                    let boundary = |p: usize| p == disk.len() || (disk[p] & 0xC0) != 0x80;
                    prop_assert!(e.0 <= e.1 && boundary(e.0) && boundary(e.1));
                } else {
                    let stray_start = disk.first().is_some_and(|b| b & 0xC0 == 0x80);
                    prop_assert!(std::str::from_utf8(&rec).is_err() || stray_start);
                }
            }
        }
    }

    #[test]
    fn no_journal_means_no_recovery() {
        assert_eq!(assess_recovery(b"disk", None), Recovery::None);
    }

    #[test]
    fn journal_matching_disk_is_not_offered() {
        // A clean exit can leave a journal whose last record equals what was saved — do not nag.
        assert_eq!(assess_recovery(b"same", Some(b"same")), Recovery::None);
    }

    #[test]
    fn diverging_unsaved_work_is_offered() {
        assert_eq!(
            assess_recovery(b"on disk", Some(b"unsaved edits")),
            Recovery::Available(b"unsaved edits".to_vec())
        );
    }
}
