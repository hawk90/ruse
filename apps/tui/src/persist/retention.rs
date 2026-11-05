//! Archive retention (RFC-0023 §9, D-064, #934): on startup, at most once a day, ruse removes archived
//! journals older than [`MAX_AGE_MS`] and keeps at most [`MAX_PER_PATH`] archives per document path, the
//! newest.
//!
//! * **What is swept.** Only regular files in `journal/archive/` whose name the sweep owns ([`archive_key`]):
//!   `<stem>@<unix ms>`, which only [`super::journal::Journals::archive`] writes, whatever the stem (a name
//!   shortened to fit `NAME_MAX` too, so no archive stays for ever), and the `<journal name>.<unix ms>` an
//!   earlier build wrote when the journal name is exactly one a ruse writes (`<base>.<16 hex>.rj`,
//!   `<base>.<16 hex>.<pid>.rj`, `unnamed.<pid>.<start ms>.rj`, a legacy `<file>.ruse-journal`). Anything else
//!   there is left alone, and so is every live journal in `journal/` (the sweep never lists that
//!   directory). A symlink is never followed or removed, and an archive directory that is itself a symlink
//!   is not swept.
//! * **Age** is the archive's modification time, which [`super::journal::Journals::archive`] sets when it
//!   moves a journal in, so it counts from the archiving, not from the journal's last write.
//! * **Per document path.** A file's journals carry its path hash, so `<base>.<16 hex>` is the path; a second
//!   session's `<name>.<pid>.rj` counts with the owner's. Unnamed buffers have no path and count together as
//!   one. A legacy sidecar's name has only the file's basename, so same-named files share a cap; a shortened
//!   name counts by its shortened stem.
//! * **Once a day.** The time of the last sweep is kept in `journal/archive-sweep` (Unix ms as text), which
//!   `:checkhealth` shows. A stamp in the future (the clock went back) counts as due.

use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use super::journal::{path_bytes, read_journal, Journals, ARCHIVE_MARK};

/// Archives older than this are removed: 14 days.
pub const MAX_AGE_MS: u64 = 14 * DAY_MS;
/// At most this many archives are kept per document path.
pub const MAX_PER_PATH: usize = 20;
/// The sweep runs at most once in this long.
pub const DAY_MS: u64 = 24 * 60 * 60 * 1000;
/// The stamp file, in the journal directory.
const STAMP: &str = "archive-sweep";

/// What a sweep removed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Swept {
    /// Archives older than [`MAX_AGE_MS`].
    pub aged: usize,
    /// Archives past [`MAX_PER_PATH`] for their path.
    pub over_cap: usize,
}

/// The archive as `:checkhealth` reports it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ArchiveStats {
    /// Archive files (by name) and their total size in bytes.
    pub count: usize,
    pub bytes: u64,
    /// When the last sweep ran (Unix ms), `None` when none has.
    pub last_sweep_ms: Option<u64>,
}

/// The grouping key of an archive file name, or `None` when the sweep does not own the name.
///
/// * `<stem>@<unix ms>` ([`ARCHIVE_MARK`]): what [`Journals::archive`] writes, for every name it is given,
///   the stem shortened when the whole would not fit `NAME_MAX`. The key is the document path part of the
///   journal name the stem is ([`journal_key`]), else the stem itself (a shortened or unknown name).
/// * `<journal name>.<unix ms>`: what an earlier build wrote (#1208), owned only when the journal name is
///   exactly one a ruse writes.
pub fn archive_key(name: &[u8]) -> Option<Vec<u8>> {
    if let Some(i) = name.iter().rposition(|&b| b == ARCHIVE_MARK) {
        let (stem, ms) = (&name[..i], &name[i + 1..]);
        if !stem.is_empty() && digits(ms, 20).is_some() {
            return Some(journal_key(stem).unwrap_or_else(|| stem.to_vec()));
        }
    }
    let (journal, ms) = rsplit_dot(name)?;
    digits(ms, 20)?;
    journal_key(journal)
}

/// The document path part of a journal name a ruse writes: `<base>.<16 hex>` for a file's (also a second
/// session's `<base>.<16 hex>.<pid>.rj`), `unnamed` for every unnamed buffer's, the whole name for a legacy
/// `<file>.ruse-journal`. `None` for any other name.
fn journal_key(journal: &[u8]) -> Option<Vec<u8>> {
    if journal.ends_with(b".ruse-journal") && journal.len() > b".ruse-journal".len() {
        return Some(journal.to_vec());
    }
    let stem = journal.strip_suffix(b".rj")?;
    let (head, last) = rsplit_dot(stem)?;
    if is_hash(last) && !head.is_empty() {
        return Some(stem.to_vec());
    }
    digits(last, 20)?;
    // `<base>.<16 hex>.<pid>`: a second session's journal of the same path.
    if let Some((base, hash)) = rsplit_dot(head) {
        if is_hash(hash) && !base.is_empty() && last.len() <= 10 {
            return Some(head.to_vec());
        }
    }
    // `unnamed.<pid>.<start ms>`.
    digits(head.strip_prefix(b"unnamed.")?, 10)?;
    Some(b"unnamed".to_vec())
}

fn rsplit_dot(s: &[u8]) -> Option<(&[u8], &[u8])> {
    let i = s.iter().rposition(|&b| b == b'.')?;
    Some((&s[..i], &s[i + 1..]))
}

/// `s` is 1 to `max` ASCII digits.
fn digits(s: &[u8], max: usize) -> Option<()> {
    (!s.is_empty() && s.len() <= max && s.iter().all(u8::is_ascii_digit)).then_some(())
}

fn is_hash(s: &[u8]) -> bool {
    s.len() == 16
        && s.iter()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(b))
}

/// The archive's files: (name key, path, modification ms, size), regular files with an archive name only.
/// Empty when the archive directory is missing, unreadable or not a real directory (a symlink).
fn archives(dir: &Path) -> Vec<(Vec<u8>, PathBuf, u64, u64)> {
    if !fs::symlink_metadata(dir).is_ok_and(|m| m.is_dir()) {
        return Vec::new();
    }
    fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter_map(|e| {
            let key = archive_key(&path_bytes(Path::new(&e.file_name())))?;
            // `DirEntry::metadata` does not follow a symlink.
            let m = e.metadata().ok().filter(fs::Metadata::is_file)?;
            let ms = m
                .modified()
                .ok()?
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_millis() as u64);
            Some((key, e.path(), ms, m.len()))
        })
        .collect()
}

/// Remove the archives older than [`MAX_AGE_MS`] at `now_ms`, then all but the newest [`MAX_PER_PATH`] per
/// document path. A file that cannot be removed is skipped.
pub fn sweep(journals: &Journals, now_ms: u64) -> Swept {
    let mut swept = Swept::default();
    let mut by_key: HashMap<Vec<u8>, Vec<(u64, PathBuf)>> = HashMap::new();
    for (key, path, ms, _) in archives(&journals.archive_dir()) {
        if now_ms.saturating_sub(ms) > MAX_AGE_MS {
            if fs::remove_file(&path).is_ok() {
                swept.aged += 1;
            }
        } else {
            by_key.entry(key).or_default().push((ms, path));
        }
    }
    for mut group in by_key.into_values() {
        // Newest first; the name (its archive ms suffix) breaks a tie.
        group.sort_by(|a, b| b.cmp(a));
        for (_, path) in group.into_iter().skip(MAX_PER_PATH) {
            if fs::remove_file(&path).is_ok() {
                swept.over_cap += 1;
            }
        }
    }
    swept
}

fn stamp_path(journals: &Journals) -> PathBuf {
    journals.dir().join(STAMP)
}

/// When the last sweep ran (Unix ms), from the stamp file.
pub fn last_sweep(journals: &Journals) -> Option<u64> {
    let raw = read_journal(&stamp_path(journals)).ok()?;
    let s = std::str::from_utf8(&raw).ok()?.trim();
    s.parse().ok()
}

/// Sweep when no sweep ran in the last day at `now_ms` (or the stamp is missing, unreadable or in the
/// future), and record the time. `None` when it was not due. Nothing is created when there is no journal
/// directory yet.
pub fn sweep_if_due(journals: &Journals, now_ms: u64) -> Option<Swept> {
    if !fs::symlink_metadata(journals.dir()).is_ok_and(|m| m.is_dir()) {
        return None;
    }
    if let Some(last) = last_sweep(journals) {
        if last <= now_ms && now_ms - last < DAY_MS {
            return None;
        }
    }
    let swept = sweep(journals, now_ms);
    let _ = write_stamp(&stamp_path(journals), now_ms);
    Some(swept)
}

/// Replace the stamp. The old one is removed first, so a symlink planted there is replaced, not followed.
fn write_stamp(path: &Path, now_ms: u64) -> io::Result<()> {
    match fs::remove_file(path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
        _ => {}
    }
    let mut opts = OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
    opts.open(path)?.write_all(format!("{now_ms}\n").as_bytes())
}

/// The archive's count, size and last sweep, for `:checkhealth`.
pub fn stats(journals: &Journals) -> ArchiveStats {
    let files = archives(&journals.archive_dir());
    ArchiveStats {
        count: files.len(),
        bytes: files.iter().map(|f| f.3).sum(),
        last_sweep_ms: last_sweep(journals),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    const NOW: u64 = 1_800_000_000_000;
    const HASH: &str = "0123456789abcdef";

    fn scratch(tag: &str) -> (Journals, PathBuf) {
        let dir = std::env::temp_dir().join(format!("ruse-retention-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let j = Journals::in_state_home(&dir.join("state"));
        fs::create_dir_all(j.archive_dir()).unwrap();
        (j, dir)
    }

    /// A file at `path` whose modification time is `ms`.
    fn put(path: &Path, ms: u64) {
        fs::write(path, b"x").unwrap();
        let f = OpenOptions::new().write(true).open(path).unwrap();
        f.set_modified(UNIX_EPOCH + Duration::from_millis(ms))
            .unwrap();
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut v: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        v.sort();
        v
    }

    #[test]
    fn archive_names_are_recognised_exactly() {
        let key = |s: &str| archive_key(s.as_bytes()).map(|k| String::from_utf8(k).unwrap());
        let doc = format!("f.txt.{HASH}");
        assert_eq!(
            key(&format!("f.txt.{HASH}.rj.1700000000000")),
            Some(doc.clone())
        );
        assert_eq!(
            key(&format!("f.txt.{HASH}.4242.rj.1700000000000")),
            Some(doc)
        );
        assert_eq!(
            key("unnamed.4242.1700000000000.rj.1700000000001").as_deref(),
            Some("unnamed")
        );
        assert_eq!(
            key("notes.md.ruse-journal.17").as_deref(),
            Some("notes.md.ruse-journal")
        );
        // What `Journals::archive` writes: `<stem>@<ms>`, keyed by the journal name's path part, or the
        // stem itself when it is not one (a name shortened to fit NAME_MAX).
        assert_eq!(
            key(&format!("f.txt.{HASH}.4242.rj@1700000000000")),
            Some(format!("f.txt.{HASH}"))
        );
        assert_eq!(key("unnamed.1.2.rj@3").as_deref(), Some("unnamed"));
        assert_eq!(key("aaaa.ruse-jou@17").as_deref(), Some("aaaa.ruse-jou"));
        for bad in [
            "@17".to_string(),
            "x@".into(),
            "x@17a".into(),
            "x@-17".into(),
            format!("f.txt.{HASH}.rj"),             // a live journal name
            format!("f.txt.{HASH}.rj.17x"),         // not a time
            format!("f.txt.{HASH}.rj."),            // empty time
            format!("f.txt.{}.rj.17", &HASH[..15]), // short hash
            format!("f.txt.{}.rj.17", HASH.to_uppercase()),
            format!(".{HASH}.rj.17"), // no basename
            "f.txt.17".into(),
            "archive-sweep".into(),
            ".ruse-journal.17".into(),
            "unnamed.x.17.rj.17".into(),
            "README".into(),
        ] {
            assert_eq!(key(&bad), None, "{bad}");
        }
    }

    /// Archives older than 14 days go; younger ones and look-alike names stay.
    #[test]
    fn old_archives_are_removed() {
        let (j, dir) = scratch("age");
        let a = j.archive_dir();
        let old = NOW - MAX_AGE_MS - 1;
        put(&a.join(format!("f.txt.{HASH}.rj.1")), old);
        put(&a.join("unnamed.1.2.rj.3"), old);
        put(&a.join("n.ruse-journal.4"), old);
        put(
            &a.join(format!("g.txt.{HASH}.rj.5")),
            NOW - MAX_AGE_MS + 1000,
        );
        put(&a.join("notes.txt"), old);
        put(&a.join(format!("f.txt.{HASH}.rj")), old);
        assert_eq!(
            sweep(&j, NOW),
            Swept {
                aged: 3,
                over_cap: 0
            }
        );
        assert_eq!(
            names(&a),
            [
                format!("f.txt.{HASH}.rj"),
                format!("g.txt.{HASH}.rj.5"),
                "notes.txt".into()
            ]
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// #934: a legacy sidecar whose name is 255 bytes is archived under a shortened name, and that archive is
    /// still swept after 14 days; look-alike names the sweep does not own stay.
    #[test]
    fn a_shortened_archive_name_is_still_swept() {
        let (j, dir) = scratch("long");
        let sidecar = dir.join(format!("{}.ruse-journal", "a".repeat(242)));
        assert_eq!(sidecar.file_name().unwrap().len(), 255);
        fs::write(&sidecar, b"legacy work").unwrap();
        let archived = j.archive(&sidecar).unwrap();
        let name = archived.file_name().unwrap().to_str().unwrap().to_string();
        assert!(
            name.len() <= 255 && !name.contains(".ruse-journal"),
            "{name}"
        );
        assert!(archive_key(name.as_bytes()).is_some(), "{name}");
        let stem = &name[..name.rfind('@').unwrap()];
        let a = j.archive_dir();
        let lookalikes = [format!("{stem}.17"), stem.to_string(), format!("{stem}@x")];
        for d in &lookalikes {
            put(&a.join(d), 0);
        }
        let ms = |p: &Path| {
            fs::metadata(p)
                .unwrap()
                .modified()
                .unwrap()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64
        };
        let at = ms(&archived);
        assert_eq!(sweep(&j, at + MAX_AGE_MS).aged, 0, "not yet 14 days old");
        assert_eq!(sweep(&j, at + MAX_AGE_MS + 1).aged, 1);
        assert!(!archived.exists());
        let mut want = lookalikes.to_vec();
        want.sort();
        assert_eq!(names(&a), want);
        let _ = fs::remove_dir_all(&dir);
    }

    /// At most 20 archives per document path, the newest kept; a second session's counts with the owner's.
    #[test]
    fn each_path_keeps_its_newest_twenty() {
        let (j, dir) = scratch("cap");
        let a = j.archive_dir();
        for i in 0..15u64 {
            put(
                &a.join(format!("f.txt.{HASH}.rj.{i}")),
                NOW - 100_000 + i * 10,
            );
            put(
                &a.join(format!("f.txt.{HASH}.77.rj.{i}")),
                NOW - 100_000 + i * 10 + 5,
            );
        }
        // Another path's 3 are not counted with them.
        let other = "fedcba9876543210";
        for i in 0..3u64 {
            put(&a.join(format!("f.txt.{other}.rj.{i}")), NOW - 200_000);
        }
        let s = sweep(&j, NOW);
        assert_eq!(s.over_cap, 10);
        let left = names(&a);
        assert_eq!(left.len(), 23);
        // The oldest five of each name went.
        for i in 0..5 {
            assert!(!left.contains(&format!("f.txt.{HASH}.rj.{i}")));
            assert!(!left.contains(&format!("f.txt.{HASH}.77.rj.{i}")));
        }
        assert!(left.contains(&format!("f.txt.{HASH}.rj.5")));
        let _ = fs::remove_dir_all(&dir);
    }

    /// The sweep runs once a day and records when; a stamp in the future counts as due.
    #[test]
    fn the_sweep_runs_at_most_once_a_day() {
        let (j, dir) = scratch("stamp");
        let a = j.archive_dir();
        assert_eq!(stats(&j).last_sweep_ms, None);
        assert_eq!(sweep_if_due(&j, NOW), Some(Swept::default()));
        assert_eq!(last_sweep(&j), Some(NOW));
        put(&a.join(format!("f.txt.{HASH}.rj.1")), 0);
        assert_eq!(sweep_if_due(&j, NOW + DAY_MS - 1), None);
        assert_eq!(names(&a).len(), 1);
        assert_eq!(sweep_if_due(&j, NOW + DAY_MS).map(|s| s.aged), Some(1));
        assert_eq!(last_sweep(&j), Some(NOW + DAY_MS));
        // The clock went back: due again.
        assert!(sweep_if_due(&j, NOW).is_some());
        let _ = fs::remove_dir_all(&dir);
    }

    /// No journal directory: nothing is created.
    #[test]
    fn no_journal_directory_no_sweep() {
        let dir = std::env::temp_dir().join(format!("ruse-retention-none-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let j = Journals::in_state_home(&dir);
        assert_eq!(sweep_if_due(&j, NOW), None);
        assert!(!dir.exists());
    }

    /// Live journals in `journal/` are never touched, whoever owns them and however old.
    #[test]
    fn live_journals_are_untouched() {
        let (j, dir) = scratch("live");
        let live = [
            format!("f.txt.{HASH}.rj"),
            format!("f.txt.{HASH}.4242.rj"),
            "unnamed.1.2.rj".into(),
            // Names that would be archive names, but in the live directory.
            format!("f.txt.{HASH}.rj.1"),
        ];
        for n in &live {
            put(&j.dir().join(n), 0);
        }
        // One beyond every bound in the archive, so the sweep does run.
        put(&j.archive_dir().join(format!("f.txt.{HASH}.rj.1")), 0);
        assert_eq!(sweep_if_due(&j, NOW).map(|s| s.aged), Some(1));
        let mut want: Vec<String> = live.to_vec();
        want.extend(["archive".into(), STAMP.into()]);
        want.sort();
        assert_eq!(names(j.dir()), want);
        let _ = fs::remove_dir_all(&dir);
    }

    /// A symlink in the archive is neither followed nor removed, and a symlinked archive directory is not
    /// swept.
    #[cfg(unix)]
    #[test]
    fn symlinks_are_not_followed() {
        use std::os::unix::fs::symlink;
        let (j, dir) = scratch("link");
        let outside = dir.join("outside");
        fs::create_dir_all(&outside).unwrap();
        let target = outside.join(format!("f.txt.{HASH}.rj.1"));
        put(&target, 0);
        let link = j.archive_dir().join(format!("f.txt.{HASH}.rj.2"));
        symlink(&target, &link).unwrap();
        assert_eq!(sweep(&j, NOW), Swept::default());
        assert!(target.exists() && fs::symlink_metadata(&link).is_ok());
        // The archive directory itself a link to `outside`.
        fs::remove_dir_all(j.archive_dir()).unwrap();
        symlink(&outside, j.archive_dir()).unwrap();
        assert_eq!(sweep(&j, NOW), Swept::default());
        assert!(target.exists());
        assert_eq!(stats(&j).count, 0);
        // A symlinked stamp is replaced, not written through.
        let decoy = outside.join("decoy");
        fs::write(&decoy, b"keep").unwrap();
        symlink(&decoy, j.dir().join(STAMP)).unwrap();
        sweep_if_due(&j, NOW);
        assert_eq!(fs::read(&decoy).unwrap(), b"keep");
        assert_eq!(last_sweep(&j), Some(NOW));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn stats_count_the_archive() {
        let (j, dir) = scratch("stats");
        let a = j.archive_dir();
        put(&a.join(format!("f.txt.{HASH}.rj.1")), NOW);
        fs::write(a.join(format!("g.txt.{HASH}.rj.2")), b"abc").unwrap();
        put(&a.join("other"), NOW);
        assert_eq!(
            stats(&j),
            ArchiveStats {
                count: 2,
                bytes: 4,
                last_sweep_ms: None
            }
        );
        let _ = fs::remove_dir_all(&dir);
    }
}
