//! Durable persistence & crash recovery (F-008, `docs/design/persistence-and-recovery.md`).
//!
//!   * [`atomic`]   — temp + fsync + rename + directory fsync (durable "Saved"). (#1)
//!   * [`encoding`] — detect the original EOL/BOM and restore it on save. (#2)
//!   * [`rj01`]     — the per-transaction journal record format and its replay (RFC-0023, D-064). (#4)
//!   * [`journal`]  — where journals live, the writer, the crash-path rescue, the open-time offer. (#4)
//!   * [`rjs0`]     — the legacy snapshot journal, read once and archived.
//!   * [`diff`]     — the line-based unified diff the recovery prompt's `d` shows.
//!   * [`disk`]     — whether a file still holds what ruse read or saved (`[disk changed]`, RFC-0023 §8).
//!   * [`Recovery`] — the open-time offer; recovery never auto-overwrites. (#3, #4)
//!
//! The format and the decisions are pure over bytes, so the persistence contract is unit-testable; only
//! the journal's thin file wrappers and the atomic save touch the disk.

use std::path::PathBuf;

use ruse_core::{BytePos, ExCmd, TransactionOrigin};

pub mod atomic;
pub mod backup;
pub mod diff;
pub mod disk;
pub mod encoding;
pub mod journal;
pub mod retention;
pub mod rj01;
pub mod rjs0;

/// What a document's journal offers at open time (RFC-0023 §7), once compared against the disk text. The
/// prompt renders it ([`crate::ui::recovery`]); the original file is never written here or there.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Recovery {
    /// No journal, or one whose text is the disk text: nothing to recover.
    None,
    /// Unsaved changes from a session that is gone, differing from the disk text.
    Available(Box<Available>),
    /// The journal's owner is a ruse process still running on this host (its pid answers `kill(pid, 0)` and
    /// its start time matches the Header's): another session is editing the file (#1193).
    InUse {
        owner: rj01::Owner,
        journal: PathBuf,
        /// Unsaved work a second session that is gone left beside the live one's journal (#1210): the
        /// in-use prompt offers it too, so it need not wait until the live session ends.
        crashed: Option<Box<Available>>,
        /// A journal beside it that could not be offered: why (the status line, after the answer).
        note: Option<String>,
    },
    /// A journal exists but cannot be offered: from a newer major version (kept), not readable (kept), or a
    /// large-file journal whose disk text changed (archived, RFC-0023 §5). `reason` is the status line.
    Unreplayable { reason: String },
}

/// An [`Recovery::Available`] offer: what the prompt shows and what each answer acts on.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Available {
    /// The text the journal replays to.
    pub recovered: Vec<u8>,
    /// What the journal's edits applied to; `None` for a legacy RJS0 snapshot.
    pub base: Option<rj01::Base>,
    /// Edit records after the base; `None` for a legacy snapshot, which does not count them.
    pub changes: Option<usize>,
    /// The last replayed edit's time, Unix milliseconds.
    pub last_change: Option<u64>,
    /// The last sync Marker's time: what the journal is "recovered up to".
    pub up_to: Option<u64>,
    /// A torn or corrupt tail was dropped: later changes could not be recovered.
    pub lost_tail: bool,
    /// The disk file's modification time, Unix milliseconds, when it is newer than the journal (#897).
    pub stale: Option<u64>,
    /// Who wrote the journal (a session that is gone, or one on another host); `None` for a legacy one.
    pub owner: Option<rj01::Owner>,
    /// The journal file the answer acts on.
    pub journal: PathBuf,
    /// The journal is a legacy RJS0 sidecar in the project tree.
    pub legacy: bool,
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

/// The recorded Ex commands that turn the disk text into the recovered text as one change (#863, #1194):
/// they run as one undo step ([`ruse_core::ExCmd::Begin`]/`End` when there are two), so `u` goes back to the
/// disk text and a saved trace starts from it.
///
/// * The span that differs as one text edit ([`recovery_edit`]) when it is UTF-8.
/// * Otherwise the bytes as lines: a filter of every line (`ex_filter`, which carries bytes), or, for an empty
///   disk text, which has no line to filter, a read above line 1 (`ex_read 0`). Both keep the buffer's final
///   newline (an empty buffer has none), so when the recovered text's differs, a one-character text edit at
///   the end adds or removes it.
pub fn recovery_commands(disk: &[u8], recovered: &[u8]) -> Vec<ExCmd> {
    if let Some((start, end, text)) = recovery_edit(disk, recovered) {
        return vec![edit(start, end, text)];
    }
    let (first, got) = if disk.is_empty() {
        let read = ExCmd::ReadText {
            dest: 0,
            cursor_on_last: false,
            bytes: recovered.to_vec(),
        };
        (read, as_lines(recovered, false))
    } else {
        let filter = ExCmd::FilterReplace {
            first: 1,
            last: line_count(disk),
            bytes: recovered.to_vec(),
        };
        (filter, as_lines(recovered, disk.ends_with(b"\n")))
    };
    let mut cmds = vec![first];
    if got.len() < recovered.len() {
        // `got` is `recovered` without its final `\n`.
        cmds.push(edit(got.len(), got.len(), "\n".into()));
    } else if got.len() > recovered.len() {
        // `got` is `recovered` with a `\n` added.
        cmds.push(edit(recovered.len(), got.len(), String::new()));
    }
    cmds
}

fn edit(start: usize, end: usize, text: String) -> ExCmd {
    ExCmd::Edits {
        origin: TransactionOrigin::UserInput,
        edits: vec![(BytePos(start), BytePos(end), text)],
    }
}

/// What replacing a buffer's lines with `text` gives (a filter, or a read into an empty buffer): `text`'s
/// lines, ending in `\n` when `final_newline` (the buffer's) and there is a line.
fn as_lines(text: &[u8], final_newline: bool) -> Vec<u8> {
    let mut out = text.strip_suffix(b"\n").unwrap_or(text).to_vec();
    if final_newline && !out.is_empty() {
        out.push(b'\n');
    }
    out
}

/// The number of lines of `text`, as a line range counts them (a final line without `\n` counts).
pub fn line_count(text: &[u8]) -> usize {
    text.iter().filter(|&&b| b == b'\n').count()
        + usize::from(!text.is_empty() && !text.ends_with(b"\n"))
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
    fn recovery_commands_follow_the_bytes() {
        let filter = |last, bytes: &[u8]| ExCmd::FilterReplace {
            first: 1,
            last,
            bytes: bytes.to_vec(),
        };
        assert_eq!(
            recovery_commands(b"caf\xe9\n", b"caf\xe9 au lait\n"),
            [edit(4, 4, " au lait".into())]
        );
        assert_eq!(
            recovery_commands(b"x\ny\n", b"\xff\n"),
            [filter(2, b"\xff\n")]
        );
        // The disk text has no line: a read above line 1, then the final newline the read leaves out.
        assert_eq!(
            recovery_commands(b"", b"\xff\n"),
            [
                ExCmd::ReadText {
                    dest: 0,
                    cursor_on_last: false,
                    bytes: b"\xff\n".to_vec()
                },
                edit(1, 1, "\n".into())
            ]
        );
        // The final newline differs: the filter keeps the buffer's, a one-character edit fixes it.
        assert_eq!(
            recovery_commands(b"a\n", b"\xff"),
            [filter(1, b"\xff"), edit(1, 2, String::new())]
        );
        assert_eq!(
            recovery_commands(b"a", b"\xff\n"),
            [filter(1, b"\xff\n"), edit(1, 1, "\n".into())]
        );
        assert_eq!(line_count(b""), 0);
        assert_eq!(line_count(b"a\nb"), 2);
        assert_eq!(line_count(b"a\n"), 1);
    }
}
