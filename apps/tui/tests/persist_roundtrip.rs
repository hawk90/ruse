//! End-to-end proof of the F-008 persistence contract over a REAL filesystem (the pure unit tests
//! cover the byte logic; this exercises the actual atomic-save + journal IO wrappers together).
//!
//! The narrative is the acceptance list: open a CRLF file, edit, crash with unsaved work, reopen and
//! recover, then save durably and confirm the journal is cleared and the line-endings preserved.

use ruse_tui::persist::encoding::{Eol, FileFormat};
use ruse_tui::persist::{atomic, journal, Recovery};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

fn scratch(name: &str) -> PathBuf {
    static N: AtomicU32 = AtomicU32::new(0);
    let mut d = std::env::temp_dir();
    d.push(format!(
        "ruse-persist-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&d).unwrap();
    d.join(name)
}

#[test]
fn atomic_save_preserves_crlf_and_is_durable() {
    let f = scratch("dos.txt");
    // A CRLF file lands on disk as CRLF and reads back into the editor as clean LF.
    let original = b"alpha\r\nbeta\r\n";
    atomic::save(&f, original).unwrap();
    let raw = std::fs::read(&f).unwrap();
    let fmt = FileFormat::detect(&raw);
    assert_eq!(fmt.eol, Eol::Crlf);
    let buffer = fmt.to_buffer(&raw);
    assert_eq!(buffer, b"alpha\nbeta\n");

    // Edit in LF, save; the file keeps its CRLF style for every line, new ones included.
    let edited = b"alpha\nBETA\ngamma\n";
    atomic::save(&f, &fmt.to_disk(edited)).unwrap();
    assert_eq!(std::fs::read(&f).unwrap(), b"alpha\r\nBETA\r\ngamma\r\n");
}

/// The journal directory of a test's own state home, next to its files.
fn journals(near: &Path) -> journal::Journals {
    journal::Journals::in_state_home(&near.parent().unwrap().join("state"))
}

#[test]
fn crash_with_unsaved_work_is_recoverable_then_cleared_on_save() {
    let f = scratch("work.txt");
    let j = journals(&f);
    atomic::save(&f, b"saved line\n").unwrap();
    let disk = std::fs::read(&f).unwrap();

    // No journal yet → nothing to recover.
    assert_eq!(j.offer(Some(&f), &disk), Recovery::None);

    // The crash path writes the exact latest text; a later one supersedes an earlier one.
    j.write_text(Some(&f), b"saved line\nunsaved 1").unwrap();
    let journal = j
        .write_text(Some(&f), b"saved line\nunsaved 1\nunsaved 2")
        .unwrap();
    assert!(journal.starts_with(j.dir()), "outside the project tree");

    // Reopen: the journal's latest valid record differs from disk → recovery is offered.
    match j.offer(Some(&f), &disk) {
        Recovery::Available(a) => assert_eq!(a.recovered, b"saved line\nunsaved 1\nunsaved 2"),
        other => panic!("{other:?}"),
    }

    // A durable save clears the journal — there is no longer unsaved work to recover.
    atomic::save(&f, b"saved line\nunsaved 1\nunsaved 2\n").unwrap();
    j.clear(Some(&f));
    assert_eq!(j.replay(Some(&f)), None);
}

#[test]
fn per_buffer_journals_are_independent_by_path() {
    // F-007: each file buffer is journaled under ITS path, so two open files (e.g. one via `:e`) each keep
    // their own recovery — one buffer's unsaved work never leaks into the other's.
    let a = scratch("a.txt");
    let b = a.with_file_name("b.txt");
    let j = journals(&a);
    j.write_text(Some(&a), b"A unsaved").unwrap();
    j.write_text(Some(&b), b"B unsaved").unwrap();

    assert_eq!(j.replay(Some(&a)).as_deref(), Some(&b"A unsaved"[..]));
    assert_eq!(j.replay(Some(&b)).as_deref(), Some(&b"B unsaved"[..]));

    // Saving/clearing one buffer leaves the other's journal intact.
    j.clear(Some(&a));
    assert_eq!(j.replay(Some(&a)), None);
    assert_eq!(j.replay(Some(&b)).as_deref(), Some(&b"B unsaved"[..]));
}

#[test]
fn a_torn_journal_tail_never_blocks_reopening() {
    let f = scratch("torn.txt");
    let j = journals(&f);
    // A good journal, then a truncated record (a crash mid-append). Recovery must still work off the last
    // intact record rather than erroring the open.
    let path = j.write_text(Some(&f), b"rev B").unwrap();
    let mut bytes = std::fs::read(&path).unwrap();
    let mut torn = Vec::new();
    ruse_tui::persist::rj01::encode_checkpoint(&mut torn, 0, 0, b"rev C - interrupted").unwrap();
    bytes.extend_from_slice(&torn[..torn.len() - 5]);
    std::fs::write(&path, &bytes).unwrap();

    assert_eq!(j.replay(Some(&f)).as_deref(), Some(&b"rev B"[..]));
}
