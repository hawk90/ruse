//! End-to-end proof of the F-008 persistence contract over a REAL filesystem (the pure unit tests
//! cover the byte logic; this exercises the actual atomic-save + journal IO wrappers together).
//!
//! The narrative is the acceptance list: open a CRLF file, edit, crash with unsaved work, reopen and
//! recover, then save durably and confirm the journal is cleared and the line-endings preserved.

use ruse_tui::persist::encoding::{Eol, FileFormat};
use ruse_tui::persist::{assess_recovery, atomic, journal, Recovery};
use std::path::PathBuf;
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

#[test]
fn crash_with_unsaved_work_is_recoverable_then_cleared_on_save() {
    let f = scratch("work.txt");
    atomic::save(&f, b"saved line\n").unwrap();
    let disk = std::fs::read(&f).unwrap();

    // No journal yet → nothing to recover.
    assert_eq!(
        assess_recovery(&disk, journal::replay(Some(&f)).as_deref()),
        Recovery::None
    );

    // The editor throttles unsaved snapshots into the journal; simulate two, the second being the
    // latest unsaved state at the moment of the "crash".
    journal::append(Some(&f), b"saved line\nunsaved 1").unwrap();
    journal::append(Some(&f), b"saved line\nunsaved 1\nunsaved 2").unwrap();

    // Reopen: the journal's latest valid record differs from disk → recovery is offered.
    let recovered = journal::replay(Some(&f));
    assert_eq!(
        assess_recovery(&disk, recovered.as_deref()),
        Recovery::Available(b"saved line\nunsaved 1\nunsaved 2".to_vec())
    );

    // A durable save clears the journal — there is no longer unsaved work to recover.
    atomic::save(&f, b"saved line\nunsaved 1\nunsaved 2\n").unwrap();
    journal::clear(Some(&f));
    assert_eq!(journal::replay(Some(&f)), None);
}

#[test]
fn a_torn_journal_tail_never_blocks_reopening() {
    let f = scratch("torn.txt");
    // Two good records, then a truncated third (a crash mid-append). Recovery must still work off
    // the last intact record rather than erroring the open.
    let mut bytes = journal::frame(b"rev A");
    bytes.extend(journal::frame(b"rev B"));
    let torn = journal::frame(b"rev C - interrupted");
    bytes.extend_from_slice(&torn[..torn.len() - 5]);
    std::fs::write(journal::journal_path(Some(&f)), &bytes).unwrap();

    assert_eq!(journal::replay(Some(&f)).as_deref(), Some(&b"rev B"[..]));
}
