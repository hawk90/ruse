//! The `Writer` tests; they read its private state, so they live under `writer`. Split from
//! `journal.rs` (#1389).

use super::*;
use crate::persist::journal::tests::{available, insert, opened, scratch, step, unreplayable};

/// The writer starts a journal at the first change (Header + Checkpoint of the text it applied to),
/// appends Edit records, and a replay gives the current text; the journal is owner-only.
#[test]
fn writer_journals_changes_and_replays_them() {
    let (j, dir) = scratch("writer");
    let file = dir.join("f.txt");
    fs::write(&file, b"orig\n").unwrap();
    let mut d = opened(b"orig\n");
    let mut w = Writer::new(&j, &file, FileFormat::default());
    step(&mut w, &mut d);
    assert!(!w.journal().exists(), "a clean buffer has no journal");
    insert(&mut d, 0, "a");
    step(&mut w, &mut d);
    insert(&mut d, 1, "b");
    d.undo();
    d.redo();
    step(&mut w, &mut d);
    let raw = read_journal(w.journal()).unwrap();
    let r = rj01::replay(&raw, None);
    assert_eq!(r.text.as_deref(), Some(&b"aborig\n"[..]));
    assert_eq!(r.changes, 4, "two edits, an undo and a redo");
    assert!(r.last_marker_ms.is_some());
    assert_eq!(r.header.unwrap().path, path_bytes(&canonical(&file)));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(w.journal()), 0o600);
        assert_eq!(mode(j.dir()), 0o700);
    }
    // Undo back to the saved text: the journal goes (#862).
    d.undo();
    d.undo();
    step(&mut w, &mut d);
    assert!(!w.journal().exists());
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn the_sync_policy_waits_for_idle_or_a_change_count() {
    let (j, dir) = scratch("policy");
    let file = dir.join("f.txt");
    let mut d = opened(b"");
    let mut w = Writer::new(&j, &file, FileFormat::default());
    let t0 = Instant::now();
    w.absorb(&mut d, FileFormat::default(), t0);
    insert(&mut d, 0, "x");
    w.absorb(&mut d, FileFormat::default(), t0);
    assert!(w.unsynced());
    assert!(!w.due(t0), "a change alone does not sync");
    assert!(w.due(t0 + IDLE), "idle does");
    assert_eq!(w.wake_in(t0), Some(IDLE));
    for i in 1..SYNC_EVERY {
        insert(&mut d, i, "x");
    }
    w.absorb(&mut d, FileFormat::default(), t0);
    assert!(w.due(t0), "{SYNC_EVERY} changes do");
    w.flush(&d).unwrap();
    assert!(!w.unsynced() && w.wake_in(t0).is_none());
    let _ = fs::remove_dir_all(&dir);
}

/// A journal deleted or changed under the writer is rewritten whole, never appended to blindly.
#[test]
fn a_journal_changed_under_the_writer_is_rewritten() {
    let (j, dir) = scratch("moved");
    let file = dir.join("f.txt");
    let mut d = opened(b"t\n");
    let mut w = Writer::new(&j, &file, FileFormat::default());
    step(&mut w, &mut d);
    insert(&mut d, 0, "1");
    step(&mut w, &mut d);
    fs::remove_file(w.journal()).unwrap();
    insert(&mut d, 0, "2");
    step(&mut w, &mut d);
    assert_eq!(j.replay(Some(&file)).as_deref(), Some(&b"21t\n"[..]));
    fs::write(w.journal(), b"junk").unwrap();
    insert(&mut d, 0, "3");
    step(&mut w, &mut d);
    assert_eq!(j.replay(Some(&file)).as_deref(), Some(&b"321t\n"[..]));
    let _ = fs::remove_dir_all(&dir);
}

/// A failed write keeps every record and retries; nothing is dropped (#907).
#[cfg(unix)]
#[test]
fn a_failed_write_keeps_the_records_for_the_retry() {
    use std::os::unix::fs::PermissionsExt;
    // SAFETY: geteuid has no preconditions. Root ignores file modes.
    if unsafe { libc::geteuid() } == 0 {
        return;
    }
    let (j, dir) = scratch("retry");
    let file = dir.join("f.txt");
    let mut d = opened(b"t\n");
    let mut w = Writer::new(&j, &file, FileFormat::default());
    step(&mut w, &mut d);
    insert(&mut d, 0, "1");
    step(&mut w, &mut d);
    fs::set_permissions(w.journal(), fs::Permissions::from_mode(0o400)).unwrap();
    insert(&mut d, 0, "2");
    w.absorb(&mut d, FileFormat::default(), Instant::now());
    assert!(w.flush(&d).is_err());
    assert!(w.unsynced(), "the records stay");
    assert!(
        !w.due(Instant::now()),
        "the retry waits for the next trigger"
    );
    fs::set_permissions(w.journal(), fs::Permissions::from_mode(0o600)).unwrap();
    insert(&mut d, 0, "3");
    step(&mut w, &mut d);
    assert_eq!(j.replay(Some(&file)).as_deref(), Some(&b"321t\n"[..]));
    let _ = fs::remove_dir_all(&dir);
}

/// RFC-0023 §4: once the edits since the last Checkpoint pass 1 MiB and the Checkpoint's size, the
/// journal is rewritten as Header + Checkpoint of the current text.
#[test]
fn a_long_edit_stream_is_compacted() {
    let (j, dir) = scratch("compact");
    let file = dir.join("f.txt");
    let mut d = opened(b"small\n");
    let mut w = Writer::new(&j, &file, FileFormat::default());
    step(&mut w, &mut d);
    let chunk = "x".repeat(64 << 10);
    for _ in 0..20 {
        insert(&mut d, 0, &chunk);
        d.undo(); // each pair journals the chunk twice; the text stays small
        insert(&mut d, 0, "k");
        step(&mut w, &mut d);
    }
    let raw = read_journal(w.journal()).unwrap();
    assert!(raw.len() < 2 << 20, "compacted: {} bytes", raw.len());
    let r = rj01::replay(&raw, None);
    assert_eq!(r.text.as_deref(), Some(d.bytes()));
    assert!(
        r.changes < 60,
        "the history before the compaction is gone: {}",
        r.changes
    );
    let _ = fs::remove_dir_all(&dir);
}

/// A rename moves the journal to the new name with a Meta rename; a `:cd` respelling changes nothing.
#[test]
fn a_rename_moves_the_journal() {
    let (j, dir) = scratch("rename");
    let (a, b) = (dir.join("a.txt"), dir.join("b.txt"));
    let mut d = opened(b"t\n");
    let mut w = Writer::new(&j, &a, FileFormat::default());
    step(&mut w, &mut d);
    insert(&mut d, 0, "1");
    step(&mut w, &mut d);
    let old = w.journal().to_path_buf();
    w.follow(&j, &dir.join(".").join("a.txt"));
    assert_eq!(w.journal(), old);
    w.follow(&j, &b);
    insert(&mut d, 0, "2");
    step(&mut w, &mut d);
    assert!(!old.exists());
    let r = rj01::replay(&read_journal(&j.path_for(&b)).unwrap(), None);
    assert_eq!(r.text.as_deref(), Some(&b"21t\n"[..]));
    assert_eq!(r.header.unwrap().path, path_bytes(&canonical(&b)));
    let _ = fs::remove_dir_all(&dir);
}

/// A Checkpoint drain (recording just turned on with the buffer modified, as for a recovered buffer,
/// or after `:e!`) writes the journal whole.
#[test]
fn a_checkpoint_drain_writes_the_whole_text() {
    let (j, dir) = scratch("ckpt");
    let file = dir.join("f.txt");
    let mut d = opened(b"disk\n");
    insert(&mut d, 0, "rec ");
    let mut w = Writer::new(&j, &file, FileFormat::default());
    step(&mut w, &mut d);
    assert_eq!(j.replay(Some(&file)).as_deref(), Some(&b"rec disk\n"[..]));
    let _ = fs::remove_dir_all(&dir);
}

/// RFC-0023 §5: a document over 32 MiB journals against its disk text; a changed disk file refuses,
/// and the journal is archived.
#[test]
fn a_large_document_journals_against_the_disk_text() {
    let (j, dir) = scratch("large");
    let file = dir.join("big.txt");
    // Buffer text is line-terminated (RFC-0024).
    let mut disk = vec![b'a'; rj01::LARGE_FILE + 1];
    *disk.last_mut().unwrap() = b'\n';
    let mut d = opened(&disk);
    let mut w = Writer::new(&j, &file, FileFormat::default());
    step(&mut w, &mut d);
    insert(&mut d, 0, "+");
    step(&mut w, &mut d);
    let raw = read_journal(w.journal()).unwrap();
    assert!(
        raw.len() < 4096,
        "no Checkpoint of the text: {} bytes",
        raw.len()
    );
    assert!(matches!(
        rj01::replay(&raw, None).header.unwrap().base,
        Base::Disk { .. }
    ));
    // The writer is this process: the journal is offered (it is not another session's).
    let text = available(j.offer(Some(&file), &disk)).recovered;
    assert!(text.len() == disk.len() + 1 && text[0] == b'+');
    let mut changed = disk.clone();
    changed[10] = b'b';
    let n = unreplayable(j.offer(Some(&file), &changed));
    assert!(
        n.contains("changed on disk") && n.contains("1 changes"),
        "{n}"
    );
    assert!(!w.journal().exists(), "archived");
    assert_eq!(fs::read_dir(j.archive_dir()).unwrap().count(), 1);
    let _ = fs::remove_dir_all(&dir);
}

/// #1203: the base hash of a large document starts when the writer first sees the clean text, and the
/// first change only buffers its record behind a held Header; the flush writes the Header and its
/// `DiskBase` first, waiting for the hash if it still runs. A risky operation's flush always writes.
#[test]
fn a_large_base_is_hashed_off_the_input_path() {
    let (j, dir) = scratch("large-hash");
    let file = dir.join("big.txt");
    // Buffer text is line-terminated (RFC-0024).
    let mut disk = vec![b'a'; rj01::LARGE_FILE + 1];
    *disk.last_mut().unwrap() = b'\n';
    let mut d = opened(&disk);
    let mut w = Writer::new(&j, &file, FileFormat::default());
    w.absorb(&mut d, FileFormat::default(), Instant::now());
    assert!(w.hashing.is_some(), "hashing began at the clean text");
    // Another clean frame reuses that hash rather than start another one.
    let first = w.hashing.as_ref().map(|h| Arc::as_ptr(&h.base));
    w.absorb(&mut d, FileFormat::default(), Instant::now());
    assert_eq!(w.hashing.as_ref().map(|h| Arc::as_ptr(&h.base)), first);
    insert(&mut d, 0, "+");
    let now = Instant::now();
    w.absorb(&mut d, FileFormat::default(), now);
    assert!(w.held.is_some() && w.hashing.is_none());
    assert!(!w.journal().exists(), "nothing is on disk before the hash");
    if w.hash_running() {
        assert!(!w.due(now + IDLE), "not due while the hash runs");
        assert!(w.wake_in(now + IDLE) >= Some(HASH_POLL));
    }
    w.flush(&d).unwrap();
    assert!(w.held.is_none());
    let raw = read_journal(w.journal()).unwrap();
    let r = rj01::replay(&raw, Some(&disk));
    assert!(matches!(
        r.header.unwrap().base,
        Base::Disk { len, sha256 } if len == disk.len() as u64 && sha256 == rj01::sha256(&disk)
    ));
    assert_eq!(
        r.text.as_deref().map(|t| (t.len(), t[0])),
        Some((disk.len() + 1, b'+'))
    );
    let _ = fs::remove_dir_all(&dir);
}
