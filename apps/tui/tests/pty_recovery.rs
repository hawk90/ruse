//! The three-way recovery prompt on a real PTY (RFC-0023 §7, #933, #1193): every key of the prompt, the diff
//! view, the archive that `e` and `k` leave, and the in-use prompt a second live ruse gets. The child's
//! `XDG_STATE_HOME` is the scratch directory's, so no test touches the user's journals.

#![cfg(unix)]

mod support;

use std::path::{Path, PathBuf};
use std::time::Duration;

use support::{wait_until, Editor, Scratch};

const ESC: &str = "\u{1b}";
const PROMPT: &str = "Recovery journal for";
/// The key line, as an 80-column terminal wraps it.
const KEYS: &str = "[r] recover   [d] show diff   [e] edit the disk file   [k] discard (archived)";
const IN_USE: &str = "E325: ATTENTION";
/// The in-use prompt's key line.
const IN_USE_KEYS: &str = "[o] open read-only   [e] edit anyway   [q] quit";

fn f_txt() -> &'static Path {
    Path::new("f.txt")
}

/// `f.txt` on disk, and a crashed session's journal of unsaved work for it.
fn crashed(dir: &Scratch) -> (PathBuf, PathBuf) {
    let file = dir.file("f.txt", b"one\ntwo\nthree\n");
    let journal = dir
        .journals()
        .write_crashed(Some(&file), b"one\nTWO\nthree\nfour\n")
        .unwrap();
    (file, journal)
}

fn archived(dir: &Scratch) -> usize {
    std::fs::read_dir(dir.journals().archive_dir()).map_or(0, |d| d.count())
}

/// The screen output since `from` bytes.
fn since(ed: &Editor, from: usize) -> String {
    ed.screen_bytes()[from..].to_string()
}

/// Wait until the output after `from` holds every one of `wants`, in any order; returns that output.
fn wait_for_all(ed: &mut Editor, from: usize, wants: &[&str]) -> String {
    for want in wants {
        ed.wait_for_after(from, want);
    }
    since(ed, from)
}

#[test]
fn the_prompt_shows_the_rfc_keys_and_ignores_every_other_key() {
    let dir = Scratch::new();
    let (file, journal) = crashed(&dir);
    let mut ed = Editor::start(&dir, &[f_txt()]);
    wait_for_all(&mut ed, 0, &[PROMPT, KEYS, "[q] quit"]);
    // The old prompt's `y` and `D`, Enter, Space and others: nothing happens (#846).
    ed.keys("yDxR \r\u{7}o");
    assert!(ed.is_running() && journal.exists());
    assert_eq!(archived(&dir), 0);
    ed.keys(ESC);
    assert_eq!(ed.exit_status(), 0, "Esc is q");
    assert!(journal.exists(), "Esc keeps the journal where it is");
    assert_eq!(archived(&dir), 0);
    assert_eq!(std::fs::read(&file).unwrap(), b"one\ntwo\nthree\n");
}

/// `d` shows a unified diff of the disk text against the recovered text, read-only; `q` goes back to the
/// prompt, and `r` then recovers.
#[test]
fn diff_then_recover() {
    let dir = Scratch::new();
    let (file, journal) = crashed(&dir);
    let mut ed = Editor::start(&dir, &[f_txt()]);
    ed.wait_for(KEYS);
    let before = ed.mark();
    ed.keys("d");
    wait_for_all(
        &mut ed,
        before,
        &[
            "--- f.txt (on disk)",
            "+++ f.txt (recovered)",
            "@@ -1,3 +1,4 @@",
            "-two",
            "+TWO",
            "+four",
            "-- recovery diff",
        ],
    );
    // Keys in the view never edit anything; `r` here is not the prompt's `r`.
    ed.keys("rjk");
    assert!(ed.is_running() && journal.exists());
    ed.keys_then_wait_for("q", KEYS); // back at the prompt
    ed.keys("r:w\r:q\r");
    assert_eq!(ed.exit_status(), 0);
    assert_eq!(std::fs::read(&file).unwrap(), b"one\nTWO\nthree\nfour\n");
    assert!(!journal.exists(), "the save made it redundant");
}

/// `e` opens the disk file, keeps the old journal in the archive, and new edits get a fresh journal.
#[test]
fn edit_the_disk_file_archives_and_journals_afresh() {
    let dir = Scratch::new();
    let (file, journal) = crashed(&dir);
    let mut ed = Editor::start(&dir, &[f_txt()]);
    ed.wait_for(KEYS);
    ed.keys("e");
    ed.sync(); // the editor is open on the disk file
    assert!(!journal.exists(), "no live journal");
    assert_eq!(archived(&dir), 1, "the old one is archived");
    // The buffer is the disk text, and clean: `:q` quits... after a fresh edit is journaled.
    ed.keys(&format!("ggiNEW {ESC}"));
    ed.sync();
    ed.signal(libc::SIGTERM);
    assert_eq!(ed.exit_status(), -libc::SIGTERM);
    assert_eq!(
        dir.journals().replay(Some(&file)).as_deref(),
        Some(&b"NEW one\ntwo\nthree\n"[..]),
        "a fresh journal of the new edit"
    );
    assert_eq!(archived(&dir), 1);
    assert_eq!(std::fs::read(&file).unwrap(), b"one\ntwo\nthree\n");
}

/// `k` discards: the disk file opens and the journal is archived, never deleted; nothing live remains.
#[test]
fn discard_archives_and_leaves_no_live_journal() {
    let dir = Scratch::new();
    let (file, journal) = crashed(&dir);
    let mut ed = Editor::start(&dir, &[f_txt()]);
    ed.wait_for(KEYS);
    ed.keys_then_wait_for("k", "recovery discarded");
    ed.keys(":q\r");
    assert_eq!(ed.exit_status(), 0, "the disk text is clean");
    assert!(!journal.exists());
    assert_eq!(archived(&dir), 1);
    let entry = std::fs::read_dir(dir.journals().archive_dir())
        .unwrap()
        .next()
        .unwrap()
        .unwrap();
    assert_eq!(
        ruse_tui::persist::rj01::replay(&std::fs::read(entry.path()).unwrap(), None)
            .text
            .as_deref(),
        Some(&b"one\nTWO\nthree\nfour\n"[..]),
        "the archive holds the work"
    );
    assert_eq!(std::fs::read(&file).unwrap(), b"one\ntwo\nthree\n");
    // The next start offers nothing.
    let mut ed2 = Editor::start(&dir, &[f_txt()]);
    assert!(!ed2.wait_for("three").contains(PROMPT));
}

/// Start ruse A on f.txt with an unsaved edit synced to its journal (`:!true` syncs before running).
fn first_session(dir: &Scratch) -> (Editor, PathBuf) {
    let file = dir.file("f.txt", b"disk\n");
    let journal = dir.journals().path_for(&file);
    let mut a = Editor::start(dir, &[f_txt()]);
    a.keys(&format!("iA {ESC}:!true\r\r"));
    assert!(
        wait_until(Duration::from_secs(5), || journal.exists()),
        "A journals its edit"
    );
    (a, file)
}

/// #1193: a second ruse on a file a live ruse is editing gets the in-use prompt, not a recovery offer. `o`
/// opens it read-only (`:w` is E45, `:w!` writes), `e` edits anyway, `q` and `Esc` open nothing, and other
/// keys do nothing. The second session never deletes or overwrites the first one's journal.
#[test]
fn a_second_live_session_gets_the_in_use_prompt() {
    let dir = Scratch::new();
    let (mut a, file) = first_session(&dir);
    let journal = dir.journals().path_for(&file);
    let theirs = std::fs::read(&journal).unwrap();

    // `q`, after keys that are not choices (the recovery prompt's `r`, `k`).
    let mut b = Editor::start(&dir, &[f_txt()]);
    let screen = wait_for_all(&mut b, 0, &[IN_USE, IN_USE_KEYS]);
    assert!(!screen.contains(PROMPT), "not offered for recovery");
    b.keys("rkd\r");
    assert!(b.is_running());
    b.keys("q");
    assert_eq!(b.exit_status(), 0);
    let mut b = Editor::start(&dir, &[f_txt()]);
    b.wait_for(IN_USE_KEYS);
    b.keys(ESC);
    assert_eq!(b.exit_status(), 0, "Esc is q");

    // `o`: read-only.
    let mut b = Editor::start(&dir, &[f_txt()]);
    b.wait_for(IN_USE_KEYS);
    b.keys("o");
    b.keys_then_wait_for(&format!("iB {ESC}:w\r"), "E45");
    assert_eq!(std::fs::read(&file).unwrap(), b"disk\n");
    // Its edit is journaled beside A's (`:!` syncs first), and `:q!` drops that journal, not A's.
    b.keys(":!true\r\r");
    let beside = || dir.journals().candidates(&file).len();
    assert!(wait_until(Duration::from_secs(5), || beside() == 2));
    b.keys(":q!\r");
    assert_eq!(b.exit_status(), 0);
    assert_eq!(beside(), 1, "B's own journal is gone");
    assert_eq!(
        std::fs::read(&journal).unwrap(),
        theirs,
        "A's journal is untouched"
    );

    // `e`: edit anyway; the edit is journaled beside A's, and a save and a quit leave A's alone.
    let mut b = Editor::start(&dir, &[f_txt()]);
    b.wait_for(IN_USE_KEYS);
    b.keys(&format!("eiB {ESC}"));
    b.sync();
    b.signal(libc::SIGTERM);
    assert_eq!(b.exit_status(), -libc::SIGTERM);
    assert_eq!(std::fs::read(&journal).unwrap(), theirs, "not overwritten");
    let mine: Vec<_> = dir
        .journals()
        .candidates(&file)
        .into_iter()
        .skip(1)
        .collect();
    assert_eq!(mine.len(), 1, "B's own journal beside A's: {mine:?}");
    // B is gone now: its journal is a crash, but A's live one still wins.
    let mut b = Editor::start(&dir, &[f_txt()]);
    // The key line now also offers `[r]` for B's journal.
    wait_for_all(&mut b, 0, &[IN_USE, "[r] recover its changes   [q] quit"]);
    b.keys("e:w\r:q\r");
    assert_eq!(b.exit_status(), 0);
    assert_eq!(
        std::fs::read(&journal).unwrap(),
        theirs,
        "a save of B leaves A's journal"
    );

    // The `:e` path asks the same.
    dir.file("other.txt", b"x\n");
    let mut b = Editor::start(&dir, &[Path::new("other.txt")]);
    b.keys_then_wait_for(":e f.txt\r", IN_USE);
    b.keys_then_wait_for("q", "not opened");
    b.keys(":q\r");
    assert_eq!(b.exit_status(), 0);

    // A still owns its journal and drops it on `:q!`.
    assert!(journal.exists());
    a.keys(":q!\r");
    assert_eq!(a.exit_status(), 0);
    assert!(!journal.exists());
}

/// #1210: a second session that crashed while a live one edits the file left its journal beside the live
/// one's. The in-use prompt names that work and offers `r`, which asks the recovery prompt for it; `r`
/// there recovers it into this session, which then edits beside the live one. The gone session's journal
/// is archived, the live one's is untouched.
#[test]
fn the_in_use_prompt_offers_a_gone_second_sessions_work() {
    let dir = Scratch::new();
    let (mut a, file) = first_session(&dir);
    let journal = dir.journals().path_for(&file);
    let theirs = std::fs::read(&journal).unwrap();
    let beside = dir
        .journals()
        .write_crashed_beside(&file, b"gone work\n")
        .unwrap();

    // `q` still opens nothing and keeps both journals.
    let mut b = Editor::start(&dir, &[f_txt()]);
    wait_for_all(
        &mut b,
        0,
        &[
            IN_USE,
            "Another session that is gone (ruse pid ",
            "[r] recover its changes",
        ],
    );
    b.keys("q");
    assert_eq!(b.exit_status(), 0);
    assert!(beside.exists());

    // `r`, then the recovery prompt's `r`.
    let mut b = Editor::start(&dir, &[f_txt()]);
    b.wait_for("[r] recover its changes");
    // Each step waits for what it shows: adopting the journal fsyncs, which can keep the screen quiet past
    // the harness's settle time on a loaded runner.
    let before = b.mark();
    b.keys("r");
    wait_for_all(&mut b, before, &[PROMPT, KEYS]);
    let before = b.mark();
    b.keys("r");
    wait_for_all(&mut b, before, &["· also open in", "gone work"]);
    assert!(!beside.exists(), "the gone session's journal is archived");
    assert_eq!(archived(&dir), 1);
    assert_eq!(
        std::fs::read(&journal).unwrap(),
        theirs,
        "A's journal untouched"
    );
    b.keys(":w\r:q\r");
    assert_eq!(b.exit_status(), 0);
    assert_eq!(std::fs::read(&file).unwrap(), b"gone work\n");
    a.keys(":q!\r");
    assert_eq!(a.exit_status(), 0);
}

/// #1194: recover `rec` over `f.txt` holding `disk`, and check it opens as the disk text plus one undoable
/// change: `:q` refuses, a saved trace replays from the disk text to the recovered bytes, and `u` goes back
/// to the disk text.
fn recovers_undoably(disk: &[u8], rec: &[u8]) {
    let dir = Scratch::new();
    let file = dir.file("f.txt", disk);
    dir.journals().write_crashed(Some(&file), rec).unwrap();
    let mut ed = Editor::start(&dir, &[f_txt()]);
    ed.wait_for(KEYS);
    ed.keys("r");
    ed.keys_then_wait_for(":q\r", "E37");
    assert!(ed.is_running());
    ed.keys(":trace save t.trace\r");
    let trace = dir.path("t.trace");
    assert!(wait_until(Duration::from_secs(5), || trace.exists()));
    let out = support::run_detached(
        &["--replay", trace.to_str().unwrap(), file.to_str().unwrap()],
        std::process::Stdio::null(),
        std::process::Stdio::piped(),
    );
    assert_eq!(out.status, 0, "replay: {}", out.stderr);
    // The replay prints what `:w` writes (#690): a file read with a final newline keeps `'endofline'`, so
    // a recovered text without one is written with it.
    let mut written = rec.to_vec();
    if !rec.is_empty() && !rec.ends_with(b"\n") && (disk.is_empty() || disk.ends_with(b"\n")) {
        written.push(b'\n');
    }
    assert_eq!(out.stdout, written);
    ed.keys("u:q\r");
    assert_eq!(
        ed.exit_status(),
        0,
        "`u` reached the disk text: the buffer is clean"
    );
    assert_eq!(std::fs::read(&file).unwrap(), disk);
}

/// #1194: a change that is not UTF-8 goes through a line filter of the bytes.
#[test]
fn a_non_utf8_recovery_is_undoable_and_traced_from_the_disk_text() {
    recovers_undoably(b"caf\xe9\nend\n", b"caf\xe9 au lait \xff\nend\n");
}

/// #1194: an empty disk file has no line to filter: the bytes are read above line 1, then the final
/// newline the read leaves out is added, in the same undo step.
#[test]
fn a_non_utf8_recovery_of_an_empty_file_is_undoable() {
    recovers_undoably(b"", b"caf\xe9\n\xff\n");
}

/// #1194: a non-UTF-8 change that also drops the final newline: the filter keeps the buffer's, and a
/// one-character edit in the same undo step removes it.
#[test]
fn a_non_utf8_recovery_that_drops_the_final_newline_is_undoable() {
    recovers_undoably(b"caf\xe9\nend\n", b"caf\xe9\nend \xff");
}

/// RFC-0023 §9 (#934): startup sweeps the archive once a day. An archive past 14 days goes, a young one and
/// a file that is not an archive stay, a crashed session's live journal is still offered, and
/// `:checkhealth` reports the archive and the sweep.
#[test]
fn startup_sweeps_old_archives_and_checkhealth_reports_them() {
    let dir = Scratch::new();
    let (_, journal) = crashed(&dir);
    let archive = dir.journals().archive_dir();
    std::fs::create_dir_all(&archive).unwrap();
    let name = journal.file_name().unwrap().to_str().unwrap().to_string();
    let aged = |p: &Path, days: u64| {
        std::fs::write(p, b"old work").unwrap();
        let t = std::time::SystemTime::now() - Duration::from_secs(days * 86_400);
        std::fs::File::options()
            .write(true)
            .open(p)
            .unwrap()
            .set_modified(t)
            .unwrap();
    };
    let old = archive.join(format!("{name}@1"));
    let young = archive.join(format!("{name}.2"));
    let other = archive.join("notes.txt");
    aged(&old, 15);
    aged(&young, 13);
    aged(&other, 30);
    // The live journal is as old as the oldest archive.
    std::fs::File::options()
        .write(true)
        .open(&journal)
        .unwrap()
        .set_modified(std::time::SystemTime::now() - Duration::from_secs(30 * 86_400))
        .unwrap();
    let mut ed = Editor::start(&dir, &[f_txt()]);
    ed.wait_for(KEYS);
    assert!(!old.exists(), "past 14 days: removed");
    assert!(young.exists() && other.exists() && journal.exists());
    assert!(dir.journals().dir().join("archive-sweep").exists());
    ed.keys("q");
    assert_eq!(ed.exit_status(), 0);
    // `k` archives the journal; the archive now holds two, and the second start does not sweep again.
    let mut ed = Editor::start(&dir, &[f_txt()]);
    ed.wait_for(KEYS);
    ed.keys_then_wait_for("k", "recovery discarded");
    ed.keys_then_wait_for(":checkhealth\r", "OK   journal archive: 2 files, ");
    ed.wait_for("last swept ");
    ed.keys(ESC);
    ed.keys(":q\r");
    assert_eq!(ed.exit_status(), 0);
}
