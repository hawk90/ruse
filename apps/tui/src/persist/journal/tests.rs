//! The journal tests: naming, the recovery offer and adoption, archives and the rescue; the helpers the
//! writer tests share. Split from `journal.rs` (#1389).

use super::*;
use ruse_core::{BytePos, DocumentId, Edit, EditList, Transaction, TransactionOrigin};

/// A state home of the test's own.
pub(super) fn scratch(tag: &str) -> (Journals, crate::TestDir) {
    let dir = crate::TestDir::new(&format!("journal-{tag}"));
    (Journals::in_state_home(&dir.join("state")), dir)
}

pub(super) fn opened(text: &[u8]) -> Document {
    let mut d = Document::new(DocumentId(1), text.to_vec());
    d.mark_saved();
    d
}

pub(super) fn insert(d: &mut Document, at: usize, s: &str) {
    let edits = EditList::new(vec![Edit::insert(BytePos(at), s)]).unwrap();
    d.apply(Transaction::new(
        d.revision(),
        edits,
        TransactionOrigin::UserInput,
    ))
    .unwrap();
    d.sync_undo();
}

/// The session's per-frame step, with the sync forced.
pub(super) fn step(w: &mut Writer, d: &mut Document) {
    w.absorb(d, FileFormat::default(), Instant::now());
    w.flush(d).unwrap();
}

#[test]
fn names_are_basename_hash_rj_and_fit_name_max() {
    let j = Journals::in_state_home(Path::new("/state"));
    let p = j.path_for_canonical(Path::new("/w/a/notes.md"));
    let name = p.file_name().unwrap().to_str().unwrap();
    assert!(
        name.starts_with("notes.md.") && name.ends_with(".rj"),
        "{name}"
    );
    assert_eq!(name.len(), "notes.md.".len() + 16 + 3);
    assert_eq!(p.parent().unwrap(), Path::new("/state/ruse/journal"));
    // Two files of one name in different trees do not collide.
    assert_ne!(p, j.path_for_canonical(Path::new("/w/b/notes.md")));
    // #1196: a 255-byte file name still gives a name within NAME_MAX, archive suffix included.
    let long = "é".repeat(127) + "x";
    assert_eq!(long.len(), 255);
    let p = j.path_for_canonical(&Path::new("/w").join(&long));
    let name = p
        .file_name()
        .unwrap()
        .to_str()
        .expect("cut at a character boundary");
    assert!(name.len() + ARCHIVE_SUFFIX <= NAME_MAX, "{}", name.len());
    // The unnamed buffer's name is fixed for the process.
    let u = j.unnamed();
    assert_eq!(u, j.unnamed());
    let pid = std::process::id().to_string();
    assert!(u
        .file_name()
        .unwrap()
        .to_str()
        .unwrap()
        .starts_with(&format!("unnamed.{pid}.")));
    assert_eq!(j.archive_dir(), Path::new("/state/ruse/journal/archive"));
}

#[test]
fn rescue_cuts_a_torn_tail_and_appends_the_exact_text() {
    let (j, dir) = scratch("rescue");
    let file = dir.join("f.txt");
    let journal = j.write_text(Some(&file), b"one\n").unwrap();
    let mut raw = fs::read(&journal).unwrap();
    let good = raw.len();
    raw.extend_from_slice(b"RJ01torn");
    fs::write(&journal, &raw).unwrap();
    rescue(&journal, Some(&file), FileFormat::default(), b"two\n", 0, 0).unwrap();
    let raw = fs::read(&journal).unwrap();
    let r = rj01::replay(&raw, None);
    assert_eq!(r.text.as_deref(), Some(&b"two\n"[..]));
    assert!(!r.lost_tail);
    assert!(raw.len() > good);
    // A journal with no intact Header is replaced.
    fs::write(&journal, b"garbage").unwrap();
    rescue(
        &journal,
        Some(&file),
        FileFormat::default(),
        b"three\n",
        0,
        0,
    )
    .unwrap();
    assert_eq!(j.replay(Some(&file)).as_deref(), Some(&b"three\n"[..]));
    let _ = fs::remove_dir_all(&dir);
}

pub(super) fn available(r: Recovery) -> Available {
    match r {
        Recovery::Available(a) => *a,
        other => panic!("not offered: {other:?}"),
    }
}

pub(super) fn unreplayable(r: Recovery) -> String {
    match r {
        Recovery::Unreplayable { reason } => reason,
        other => panic!("not a note: {other:?}"),
    }
}

/// The open-time offer: nothing for no journal or one that matches the disk; the text otherwise, with
/// what the prompt shows; a note for a newer major version, which is kept.
#[test]
fn the_offer_follows_the_journal() {
    let (j, dir) = scratch("offer");
    let file = dir.join("f.txt");
    fs::write(&file, b"disk\n").unwrap();
    assert_eq!(j.offer(Some(&file), b"disk\n"), Recovery::None);
    j.write_crashed(Some(&file), b"disk\n").unwrap();
    assert_eq!(j.offer(Some(&file), b"disk\n"), Recovery::None);
    let journal = j.write_crashed(Some(&file), b"work\n").unwrap();
    let a = available(j.offer(Some(&file), b"disk\n"));
    assert_eq!(a.recovered, b"work\n");
    assert_eq!(a.journal, journal);
    assert!(!a.legacy && !a.lost_tail);
    assert_eq!(a.base, Some(Base::Checkpoint));
    assert_eq!(a.changes, Some(0));
    assert_eq!(a.owner.unwrap().pid, GONE_PID);
    assert!(a.up_to.is_some());
    // This process's own journal (left by an earlier buffer of this session) is offered too: it is not
    // another session's.
    j.write_text(Some(&file), b"mine\n").unwrap();
    assert_eq!(
        available(j.offer(Some(&file), b"disk\n")).recovered,
        b"mine\n"
    );
    j.write_crashed(Some(&file), b"work\n").unwrap();
    // A newer major version is kept and named.
    let mut raw = fs::read(&journal).unwrap();
    raw[5] = 2;
    let crc = rj01::crc32c(&raw[..12]);
    raw[12..16].copy_from_slice(&crc.to_le_bytes());
    fs::write(&journal, &raw).unwrap();
    let n = unreplayable(j.offer(Some(&file), b"disk\n"));
    assert!(n.contains("newer ruse"), "{n}");
    assert!(journal.exists());
    // This version never writes over it: its own journal goes beside it.
    assert_ne!(claim(&journal), journal);
    let mut d = opened(b"disk\n");
    let mut w = Writer::new(&j, &file, FileFormat::default());
    step(&mut w, &mut d);
    insert(&mut d, 0, "x");
    step(&mut w, &mut d);
    assert_eq!(
        fs::read(&journal).unwrap(),
        raw,
        "the newer journal is kept as it was"
    );
    w.discard();
    let _ = fs::remove_dir_all(&dir);
}

/// A journal this process cannot read (mode 000) is reported at open and never written over: its bytes
/// and mode stay, and this session's records go beside it.
#[cfg(unix)]
#[test]
fn an_unreadable_journal_is_kept_and_written_beside() {
    use std::os::unix::fs::PermissionsExt;
    // SAFETY: geteuid has no preconditions. Root reads a mode-000 file.
    if unsafe { libc::geteuid() } == 0 {
        return;
    }
    let (j, dir) = scratch("unreadable");
    let file = dir.join("f.txt");
    fs::write(&file, b"disk\n").unwrap();
    let journal = j.write_crashed(Some(&file), b"crash work\n").unwrap();
    let bytes = fs::read(&journal).unwrap();
    fs::set_permissions(&journal, fs::Permissions::from_mode(0o000)).unwrap();
    let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
    let n = unreplayable(j.offer(Some(&file), b"disk\n"));
    assert!(n.contains("unreadable"), "{n}");
    let mut d = opened(b"disk\n");
    let mut w = Writer::new(&j, &file, FileFormat::default());
    step(&mut w, &mut d);
    insert(&mut d, 0, "x");
    step(&mut w, &mut d);
    assert_ne!(w.journal(), journal);
    assert_eq!(w.journal(), alternate(&journal, std::process::id()));
    let wrote = rescue(&journal, None, FileFormat::default(), b"crash\n", 0, 0).unwrap();
    assert_eq!(wrote, w.journal(), "the crash path writes beside it too");
    j.discard(Some(&file));
    j.clear(Some(&file));
    assert_eq!(mode(&journal), 0o000, "the mode is unchanged");
    fs::set_permissions(&journal, fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(
        fs::read(&journal).unwrap(),
        bytes,
        "the bytes are unchanged"
    );
    let r = rj01::replay(&read_journal(w.journal()).unwrap(), None);
    assert_eq!(
        r.text.as_deref(),
        Some(&b"crash\n"[..]),
        "the new records are beside it"
    );
    let _ = fs::remove_dir_all(&dir);
}

/// RFC-0023 §7 / #1193: a journal whose owner is a live process of this host with a matching start time
/// is in use; the same pid with another start time (a reused pid) is a crash; this process is neither.
#[test]
fn a_live_owner_makes_the_file_in_use() {
    let (j, dir) = scratch("inuse");
    let file = dir.join("f.txt");
    fs::write(&file, b"disk\n").unwrap();
    // A live process other than this one: a child that waits.
    let mut child = std::process::Command::new("sleep")
        .arg("30")
        .spawn()
        .unwrap();
    let pid = child.id();
    let start = process_start_ms(pid);
    let live = Owner {
        pid,
        host: owner().host.clone(),
        start_ms: start.unwrap_or(0),
    };
    j.write_as(Some(&file), b"theirs\n", &live).unwrap();
    if start.is_some() {
        assert!(is_live_other(&live));
        match j.offer(Some(&file), b"disk\n") {
            Recovery::InUse {
                owner: o,
                journal,
                crashed: None,
                note: None,
            } => {
                assert_eq!(o, live);
                assert_eq!(journal, j.path_for(&file));
            }
            other => panic!("{other:?}"),
        }
        // #1210: a crashed second session's journal beside it is offered with the in-use one.
        let beside = j.write_crashed_beside(&file, b"gone\n").unwrap();
        match j.offer(Some(&file), b"disk\n") {
            Recovery::InUse {
                owner: o,
                crashed: Some(a),
                note: None,
                ..
            } => {
                assert_eq!(o, live);
                assert_eq!(
                    (a.recovered.as_slice(), a.journal.as_path()),
                    (&b"gone\n"[..], beside.as_path())
                );
            }
            other => panic!("{other:?}"),
        }
        fs::remove_file(&beside).unwrap();
        // This session writes beside it, never over it, and never deletes it.
        let alt = claim(&j.path_for(&file));
        assert_ne!(alt, j.path_for(&file));
        assert!(alt
            .to_string_lossy()
            .ends_with(&format!(".{}.rj", std::process::id())));
        j.discard(Some(&file));
        j.clear(Some(&file));
        assert!(
            j.path_for(&file).exists(),
            "another session's journal stays"
        );
        let mut d = opened(b"disk\n");
        let mut w = Writer::new(&j, &file, FileFormat::default());
        step(&mut w, &mut d);
        insert(&mut d, 0, "x");
        step(&mut w, &mut d);
        assert_eq!(w.journal(), alt);
        assert_eq!(j.replay(Some(&file)).as_deref(), Some(&b"theirs\n"[..]));
        let wrote = rescue(
            &j.path_for(&file),
            None,
            FileFormat::default(),
            b"crash\n",
            0,
            0,
        )
        .unwrap();
        assert_eq!(wrote, alt, "the crash path also writes beside it");
        assert_eq!(j.candidates(&file), [j.path_for(&file), alt.clone()]);
        w.discard();
        assert!(!alt.exists());
    }
    // The same pid with another start time is a crash: offered for recovery.
    let reused = Owner {
        start_ms: live.start_ms.wrapping_add(60_000),
        ..live.clone()
    };
    assert!(!is_live_other(&reused));
    j.write_as(Some(&file), b"theirs\n", &reused).unwrap();
    assert_eq!(
        available(j.offer(Some(&file), b"disk\n")).recovered,
        b"theirs\n"
    );
    // Another host's journal cannot be checked: offered.
    let elsewhere = Owner {
        host: b"some-other-host".to_vec(),
        ..live.clone()
    };
    assert!(!is_live_other(&elsewhere));
    assert!(
        !is_live_other(owner()),
        "this process is not another session"
    );
    let _ = child.kill();
    let _ = child.wait();
    // Once it is gone, its journal is a crash.
    assert!(!is_live_other(&live));
    j.write_as(Some(&file), b"theirs\n", &live).unwrap();
    assert!(matches!(
        j.offer(Some(&file), b"disk\n"),
        Recovery::Available(_)
    ));
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn this_process_start_time_is_read_and_stable() {
    let me = std::process::id();
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    assert!(
        process_start_ms(me).is_some(),
        "readable where the in-use check relies on it"
    );
    if let Some(t) = process_start_ms(me) {
        assert!(t <= now_ms() && now_ms() - t < 24 * 3600 * 1000 * 30);
        assert!(process_start_ms(me).unwrap().abs_diff(t) <= START_SLACK_MS);
        assert!(owner().start_ms.abs_diff(t) <= START_SLACK_MS);
    }
    assert_eq!(process_start_ms(GONE_PID), None);
}

/// `r` takes the journal over as this process's own, and archives one of another name it came from.
#[test]
fn the_offer_carries_the_format_the_journal_recorded() {
    // #1286: a recovered buffer gets back `'endoffile'`, `'binary'` and mac from its journal, not
    // from the disk file (gone, or read before the format changed in the session).
    let (j, dir) = scratch("offer-format");
    let file = dir.join("f.txt");
    fs::write(&file, b"disk\n").unwrap();
    let fmt = FileFormat {
        eol: super::super::encoding::Eol::Cr,
        eof: true,
        binary: true,
        ..FileFormat::default()
    };
    let mut d = opened(b"disk\n");
    let mut w = Writer::new(&j, &file, fmt);
    let step = |w: &mut Writer, d: &mut Document| {
        w.absorb(d, fmt, Instant::now());
        w.flush(d).unwrap();
    };
    step(&mut w, &mut d);
    insert(&mut d, 0, "x");
    step(&mut w, &mut d);
    assert_eq!(available(j.offer(Some(&file), b"disk\n")).format, Some(fmt));
    // A format changed in the session is journaled as a Meta and offered too.
    let unix = FileFormat::default();
    w.absorb(&mut d, unix, Instant::now());
    insert(&mut d, 0, "y");
    w.absorb(&mut d, unix, Instant::now());
    w.flush(&d).unwrap();
    assert_eq!(
        available(j.offer(Some(&file), b"disk\n")).format,
        Some(unix)
    );
}

#[test]
fn adopting_a_recovery_owns_the_journal() {
    let (j, dir) = scratch("adopt");
    let file = dir.join("f.txt");
    fs::write(&file, b"disk\n").unwrap();
    j.write_crashed(Some(&file), b"work\n").unwrap();
    let a = available(j.offer(Some(&file), b"disk\n"));
    let p = j.adopt(&a, Some(&file), FileFormat::default()).unwrap();
    assert_eq!(p, j.path_for(&file));
    assert_eq!(read_owner(&p).as_ref(), Some(owner()));
    assert_eq!(j.replay(Some(&file)).as_deref(), Some(&b"work\n"[..]));
    assert!(!j.archive_dir().exists(), "nothing to archive: same name");
    // `:q!` now discards it.
    j.discard(Some(&file));
    assert!(!p.exists());
    // A second session's crashed `<name>.<pid>.rj` is archived once taken over.
    let alt = alternate(&j.path_for(&file), GONE_PID);
    j.write_crashed(Some(&file), b"alt work\n").unwrap();
    fs::rename(j.path_for(&file), &alt).unwrap();
    let a = available(j.offer(Some(&file), b"disk\n"));
    assert_eq!(a.journal, alt);
    j.adopt(&a, Some(&file), FileFormat::default()).unwrap();
    assert!(!alt.exists());
    assert_eq!(fs::read_dir(j.archive_dir()).unwrap().count(), 1);
    let _ = fs::remove_dir_all(&dir);
}

/// Two archives of one journal in the same millisecond do not replace each other.
#[test]
fn archives_never_replace_each_other() {
    let (j, dir) = scratch("archive");
    let file = dir.join("f.txt");
    for _ in 0..3 {
        let p = j.write_crashed(Some(&file), b"x").unwrap();
        j.archive(&p).unwrap();
    }
    assert_eq!(fs::read_dir(j.archive_dir()).unwrap().count(), 3);
    let _ = fs::remove_dir_all(&dir);
}

/// An archive's age counts from the archiving (RFC-0023 §9): a journal last written long ago is not
/// swept the day it is archived.
#[test]
fn archiving_restarts_the_age() {
    let (j, dir) = scratch("archive-age");
    let file = dir.join("f.txt");
    let p = j.write_crashed(Some(&file), b"x").unwrap();
    let long_ago = SystemTime::now() - Duration::from_secs(30 * 86_400);
    fs::File::options()
        .write(true)
        .open(&p)
        .unwrap()
        .set_modified(long_ago)
        .unwrap();
    let to = j.archive(&p).unwrap();
    let age = fs::metadata(&to).unwrap().modified().unwrap().elapsed();
    assert!(age.unwrap_or_default() < Duration::from_secs(3600));
    assert_eq!(super::super::retention::sweep(&j, now_ms()).aged, 0);
    assert!(to.exists());
    let _ = fs::remove_dir_all(&dir);
}

/// RFC-0023 §3: a legacy RJS0 sidecar is offered once, then archived on any answer but quit.
#[test]
fn a_legacy_sidecar_is_offered_and_archived() {
    let (j, dir) = scratch("legacy");
    let file = dir.join("f.txt");
    fs::write(&file, b"disk\n").unwrap();
    let sidecar = rjs0::sidecar_path(Some(&file));
    let mut raw = rjs0::frame(b"old");
    raw.extend(rjs0::frame(b"legacy work\n"));
    fs::write(&sidecar, &raw).unwrap();
    let a = available(j.offer(Some(&file), b"disk\n"));
    assert_eq!(a.recovered, b"legacy work\n");
    assert!(a.legacy && a.changes.is_none() && a.owner.is_none());
    j.adopt(&a, Some(&file), FileFormat::default()).unwrap();
    assert!(!sidecar.exists());
    assert_eq!(fs::read_dir(j.archive_dir()).unwrap().count(), 1);
    // The recovered text is now this session's RJ01 journal.
    assert_eq!(
        j.replay(Some(&file)).as_deref(),
        Some(&b"legacy work\n"[..])
    );
    j.discard(Some(&file));
    assert_eq!(j.offer(Some(&file), b"disk\n"), Recovery::None);
    // A sidecar holding the disk text is archived without a prompt.
    fs::write(&sidecar, rjs0::frame(b"disk\n")).unwrap();
    assert_eq!(j.offer(Some(&file), b"disk\n"), Recovery::None);
    assert!(!sidecar.exists());
    let _ = fs::remove_dir_all(&dir);
}

/// #869: a FIFO where the journal should be is never opened as one: the offer, the crash write and the
/// writer fail at once instead of blocking, and discard leaves it alone.
#[cfg(unix)]
#[test]
fn a_journal_that_is_not_a_regular_file_is_refused_without_blocking() {
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::FileTypeExt;
    let (j, dir) = scratch("fifo");
    let file = dir.join("f.txt");
    j.ensure_dir().unwrap();
    let jp = j.path_for(&file);
    let c = std::ffi::CString::new(jp.as_os_str().as_bytes()).unwrap();
    // SAFETY: `c` is a valid NUL-terminated path.
    assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
    let t = Instant::now();
    let n = unreplayable(j.offer(Some(&file), b""));
    assert!(n.contains("is not a regular file"), "{n}");
    assert!(j.write_text(Some(&file), b"x").is_err());
    let mut d = opened(b"");
    let mut w = Writer::new(&j, &file, FileFormat::default());
    w.absorb(&mut d, FileFormat::default(), t);
    insert(&mut d, 0, "x");
    w.absorb(&mut d, FileFormat::default(), t);
    assert!(w.flush(&d).is_err());
    j.discard(Some(&file));
    w.discard();
    assert!(t.elapsed() < Duration::from_secs(2), "blocked");
    assert!(fs::symlink_metadata(&jp).unwrap().file_type().is_fifo());
    let _ = fs::remove_dir_all(&dir);
}

/// #1192: the unnamed buffer's journal is offered to the next pathless start once its writer is gone,
/// and becomes that session's own on recover.
#[test]
fn an_orphaned_unnamed_journal_is_offered_and_adopted() {
    let (j, dir) = scratch("unnamed");
    j.ensure_dir().unwrap();
    // A pid no process has: above every platform's pid range.
    let orphan = j.dir().join("unnamed.2147483646.1.rj");
    let crashed = j.write_crashed(None, b"unnamed work\n").unwrap();
    fs::rename(&crashed, &orphan).unwrap();
    let a = available(j.offer(None, b""));
    assert_eq!(a.recovered, b"unnamed work\n");
    assert_eq!(a.journal, orphan);
    j.adopt(&a, None, FileFormat::default()).unwrap();
    assert!(!orphan.exists());
    assert_eq!(read_owner(&j.unnamed()).as_ref(), Some(owner()));
    // This process's own unnamed journal is never offered to itself.
    assert_eq!(j.offer(None, b""), Recovery::None);
    let _ = fs::remove_dir_all(&dir);
}
