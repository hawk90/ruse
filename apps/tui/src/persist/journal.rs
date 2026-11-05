//! Append-only recovery journal (F-008 acceptance #4: a truncated tail is discarded, not fatal).
//!
//! Unsaved work is captured as length-and-CRC framed records appended to `<file>.ruse-journal`.
//! Recovery [`replay`]s the frames and returns the LAST fully-valid one; a torn final write (partial
//! header, partial payload, or a bad CRC — exactly what a crash mid-append leaves) is discarded and
//! the prior valid record stands. A durable save deletes the journal (the work is now on disk), and so does
//! letting the changes go ([`discard`]: a normal exit, `:e!`, `:bd!`, undo back to the saved text, #862).
//! Only a regular file is ever opened as a journal (#869).
//!
//! Frame: `[magic "RJS0"][u32 len LE][u32 crc32 LE][payload]`, CRC over the length bytes and the payload,
//! so a zero-filled tail (a known outcome of power loss) is never a valid record. Before the first append in
//! a process, a torn tail is cut off, and a failed append is rolled back, so new records never land behind
//! bytes that replay stops at. Journals written before the magic existed are not read. This is the MVP-minimal
//! journal — whole-buffer snapshots, throttled by the caller ([`Throttle`]) and compacted to one frame once
//! [`MAX_FRAMES`] accumulate, so it stays bounded. The design doc's per-transaction
//! incremental records + schema version + inverse-edits are the post-MVP C-PERSIST elaboration; the
//! frame layout here is forward-compatible (a versioned header can prefix the file later).

use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

const MAGIC: [u8; 4] = *b"RJS0"; // Snapshot journal v0; the target per-transaction format is `RJ01`.
const HEADER: usize = 12; // magic + u32 len + u32 crc

/// The journal path for a document (or `.ruse-journal` for an unnamed buffer).
pub fn journal_path(doc: Option<&Path>) -> PathBuf {
    match doc {
        Some(p) => {
            let mut name = p.file_name().unwrap_or_default().to_os_string();
            name.push(".ruse-journal");
            p.with_file_name(name)
        }
        None => PathBuf::from(".ruse-journal"),
    }
}

/// Encode one framed record. A payload longer than the frame's `u32` length field can describe (4 GiB) is
/// an error (#873): it used to be truncated to the low 32 bits, writing a frame no replay could read.
pub fn frame(payload: &[u8]) -> io::Result<Vec<u8>> {
    let header = frame_header(payload.len(), payload)?;
    let mut out = Vec::with_capacity(HEADER + payload.len());
    out.extend_from_slice(&header);
    out.extend_from_slice(payload);
    Ok(out)
}

/// The 12-byte header of a frame holding `payload` (whose length is `len`, passed apart so the bound can be
/// tested without a 4 GiB buffer).
fn frame_header(len: usize, payload: &[u8]) -> io::Result<[u8; HEADER]> {
    let len = u32::try_from(len).map_err(|_| {
        io::Error::new(
            io::ErrorKind::FileTooLarge,
            format!(
                "buffer of {len} bytes is too large for a recovery journal frame (4 GiB at most)"
            ),
        )
    })?;
    let len = len.to_le_bytes();
    let mut h = [0u8; HEADER];
    h[..4].copy_from_slice(&MAGIC);
    h[4..8].copy_from_slice(&len);
    h[8..].copy_from_slice(&frame_crc(len, payload).to_le_bytes());
    Ok(h)
}

/// The CRC over a frame's length bytes and payload, computed in place: no copy of the payload (#873).
fn frame_crc(len: [u8; 4], payload: &[u8]) -> u32 {
    crc32_finish(crc32_update(crc32_update(CRC_INIT, &len), payload))
}

/// Open the journal at `path`, refusing anything but a regular file (#869): a FIFO would block the open or
/// the read, and a device such as `/dev/zero` would be read until memory runs out. `O_NONBLOCK` keeps the
/// open itself from blocking on a FIFO; it has no effect on a regular file. The type is checked on the
/// opened descriptor, so a swap between the check and the use cannot slip a FIFO in.
fn open_regular(path: &Path, opts: &mut OpenOptions) -> io::Result<fs::File> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.custom_flags(libc::O_NONBLOCK);
    }
    let f = opts.open(path)?;
    if !f.metadata()?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "is not a regular file",
        ));
    }
    Ok(f)
}

/// The most frames a journal holds before the next append compacts it (#858). Replay only ever reads the
/// last frame, so the earlier ones are history kept only as a fallback against a torn final write.
pub const MAX_FRAMES: usize = 4;

/// Frames known to be in each journal this process has appended to. A path enters on its first append
/// (after its torn tail is cut) and leaves on [`clear`], so a journal is read back at most once per
/// process instead of on every append.
static KNOWN: Mutex<Option<HashMap<PathBuf, usize>>> = Mutex::new(None);

/// Append a recovery record. Creates the journal if absent and never drops a valid record. The first
/// append to a journal in this process cuts off any torn tail first; a failed append is rolled back.
/// A journal already holding [`MAX_FRAMES`] frames is compacted: it is atomically replaced by one frame
/// with this payload, so the file stays bounded however long the session runs.
pub fn append(doc: Option<&Path>, payload: &[u8]) -> io::Result<()> {
    append_with(doc, payload, false)
}

/// [`append`] for a crash path (panic hook, fatal signal): always re-reads the journal and cuts a torn tail,
/// because the failure being rescued may have interrupted an append mid-write in this same process.
pub fn append_fresh(doc: Option<&Path>, payload: &[u8]) -> io::Result<()> {
    append_with(doc, payload, true)
}

fn append_with(doc: Option<&Path>, payload: &[u8], verify: bool) -> io::Result<()> {
    let path = journal_path(doc);
    // `try_lock`, never `lock`: a crash path may run on a thread that panicked while holding this lock.
    // When the count is unavailable the file itself is the source of truth (re-read below).
    let known = if verify {
        None
    } else {
        KNOWN
            .try_lock()
            .ok()
            .and_then(|map| map.as_ref().and_then(|m| m.get(&path).copied()))
    };
    // Refused before the journal is touched, so an oversized buffer never costs the frames already there.
    let header = frame_header(payload.len(), payload)?;
    let mut f = open_regular(
        &path,
        OpenOptions::new().create(true).read(true).append(true),
    )?;
    let count = match known {
        Some(n) => n,
        None => {
            let mut raw = Vec::new();
            io::Read::read_to_end(&mut f, &mut raw)?;
            let spans = frames(&raw);
            let valid = spans.last().map_or(0, |&(_, end)| end);
            if valid < raw.len() {
                f.set_len(valid as u64)?;
            }
            spans.len()
        }
    };
    let result = if count >= MAX_FRAMES {
        drop(f);
        // Temp + fsync + rename: a crash mid-compaction leaves the old journal intact.
        let mut whole = Vec::with_capacity(HEADER + payload.len());
        whole.extend_from_slice(&header);
        whole.extend_from_slice(payload);
        super::atomic::save(&path, &whole).map(|()| 1)
    } else {
        let before = f.metadata()?.len();
        // Header and payload are written as they are, not copied into one frame buffer first. A failure
        // between the two is rolled back below, and a crash between them leaves a torn tail replay skips.
        let written = f
            .write_all(&header)
            .and_then(|()| f.write_all(payload))
            .and_then(|()| f.sync_all());
        if written.is_err() {
            let _ = f.set_len(before);
        }
        written.map(|()| count + 1)
    };
    let now = *result.as_ref().unwrap_or(&count);
    if let Ok(mut map) = KNOWN.try_lock() {
        map.get_or_insert_with(HashMap::new).insert(path, now);
    }
    result.map(|_| ())
}

/// Remove the journal — called after a durable save, when there is no unsaved work to recover.
pub fn clear(doc: Option<&Path>) {
    let path = journal_path(doc);
    let _ = fs::remove_file(&path);
    forget(&path);
}

fn forget(path: &Path) {
    if let Ok(mut map) = KNOWN.lock() {
        if let Some(m) = map.as_mut() {
            m.remove(path);
        }
    }
}

/// Remove the journal because the buffer's changes were let go or undone (#862): a normal quit (`:q`,
/// `:q!`, `:qa!`, `:cq`), `:e!`, `:bd[!]`, or undo back to the saved text. Neovim deletes the swap file on
/// each of these exits. Unlike [`clear`], a journal this process cannot open as a regular file (unreadable,
/// a FIFO, a device) is left alone: the user was told it was not offered, so it was never seen, and it is
/// not this session's to delete.
pub fn discard(doc: Option<&Path>) {
    let path = journal_path(doc);
    if open_regular(&path, OpenOptions::new().read(true)).is_ok() {
        let _ = fs::remove_file(&path);
    }
    forget(&path);
}

/// Decides when the main loop appends a journal frame for one buffer (#858). Only a change of the
/// buffer's revision counts: idle ticks, cursor motion and re-renders never write. A frame is written
/// once `every` changes have accumulated since the last one, so a hard kill loses at most that many
/// changes; a panic or fatal signal journals the exact latest bytes separately (`crate::recover`).
#[derive(Debug, Default)]
pub struct Throttle {
    seen: Option<u64>,
    pending: u32,
    /// Whether the buffer was modified when last observed.
    dirty: bool,
}

/// What [`Throttle::tick`] asks the caller to do with the buffer's journal.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tick {
    /// Nothing this time.
    Wait,
    /// Append a frame with the buffer's current bytes.
    Append,
    /// The buffer went from modified to clean without this process knowing why (undo back to the saved
    /// text, `:e!`, a save): its journal no longer holds work, so [`discard`] it (#862).
    Discard,
}

impl Throttle {
    /// Observe the buffer at `revision`. A clean buffer (saved, or never edited) resets the count, since
    /// its journal was cleared or never needed; one that just became clean asks for its journal to go.
    pub fn tick(&mut self, revision: u64, modified: bool, every: u32) -> Tick {
        let changed = self.seen.is_some_and(|r| r != revision);
        self.seen = Some(revision);
        let was_dirty = std::mem::replace(&mut self.dirty, modified);
        if !modified {
            self.pending = 0;
            return if was_dirty { Tick::Discard } else { Tick::Wait };
        }
        if changed {
            self.pending += 1;
        }
        if self.pending >= every.max(1) {
            self.pending = 0;
            return Tick::Append;
        }
        Tick::Wait
    }
}

/// The payload of the last VALID frame in a raw journal, or `None` if there is no intact record.
/// Stops at the first torn/corrupt frame, so a crash mid-append costs at most the final record.
pub fn replay_bytes(raw: &[u8]) -> Option<Vec<u8>> {
    frames(raw)
        .last()
        .map(|&(start, end)| raw[start..end].to_vec())
}

/// [`replay_bytes`] over an owned journal: the last valid payload is cut out of `raw` in place instead of
/// copied (#873), so a replay holds one journal-sized buffer, not two.
fn replay_owned(mut raw: Vec<u8>) -> Option<Vec<u8>> {
    let (start, end) = frames(&raw).last().copied()?;
    raw.truncate(end);
    raw.drain(..start);
    Some(raw)
}

/// Payload spans of the valid frames, in order, stopping at the first torn or corrupt frame.
fn frames(raw: &[u8]) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    let mut i = 0;
    while i + HEADER <= raw.len() {
        if raw[i..i + 4] != MAGIC {
            break; // not a frame: zero fill, garbage, or a pre-magic journal
        }
        let len = [raw[i + 4], raw[i + 5], raw[i + 6], raw[i + 7]];
        let want_crc = u32::from_le_bytes([raw[i + 8], raw[i + 9], raw[i + 10], raw[i + 11]]);
        let start = i + HEADER;
        let end = match start.checked_add(u32::from_le_bytes(len) as usize) {
            Some(e) if e <= raw.len() => e,
            _ => break, // payload runs past EOF — torn tail
        };
        if frame_crc(len, &raw[start..end]) != want_crc {
            break; // corrupt frame — torn tail
        }
        spans.push((start, end));
        i = end;
    }
    spans
}

/// Read the last recoverable buffer from a document's journal, if any.
pub fn replay(doc: Option<&Path>) -> Option<Vec<u8>> {
    replay_checked(doc).ok().flatten()
}

/// [`replay`] for the open-time offer (#897): a missing journal is `Ok(None)`, but one that exists and cannot
/// be read is an error, so the caller can say so instead of opening as if there were nothing to recover.
pub fn replay_checked(doc: Option<&Path>) -> io::Result<Option<Vec<u8>>> {
    let read = open_regular(&journal_path(doc), OpenOptions::new().read(true)).and_then(|mut f| {
        let mut raw = Vec::new();
        io::Read::read_to_end(&mut f, &mut raw).map(|_| raw)
    });
    match read {
        Ok(raw) => Ok(replay_owned(raw)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Whether `doc` was modified on disk after its journal was last written (#897), so recovering would replace
/// a newer file. nvim warns the same way ("NEWER than swap file!"). False when either time is unknown.
pub fn is_stale(doc: &Path) -> bool {
    let mtime = |p: &Path| fs::metadata(p).and_then(|m| m.modified()).ok();
    matches!(
        (mtime(doc), mtime(&journal_path(Some(doc)))),
        (Some(file), Some(journal)) if file > journal
    )
}

/// The open-time recovery prompt's warning line for a stale journal ([`is_stale`]).
pub const STALE_NOTE: &str =
    "note: the file on disk is NEWER than these changes; recovering replaces it in the buffer";

/// The status line when a journal exists but [`replay_checked`] could not read it (#897).
pub fn unreadable_note(doc: Option<&Path>, err: &io::Error) -> String {
    format!(
        "recovery journal {} unreadable ({err}); not offered",
        journal_path(doc).display()
    )
}

/// Standard CRC-32 (IEEE 802.3), table computed once. No external crate; correctness over speed
/// (perf is the post-MVP pass).
#[cfg(test)]
fn crc32(bytes: &[u8]) -> u32 {
    crc32_finish(crc32_update(CRC_INIT, bytes))
}

const CRC_INIT: u32 = 0xFFFF_FFFF;

fn crc32_finish(crc: u32) -> u32 {
    crc ^ 0xFFFF_FFFF
}

/// Feed `bytes` into a running CRC-32 (start from [`CRC_INIT`], end with [`crc32_finish`]).
fn crc32_update(mut crc: u32, bytes: &[u8]) -> u32 {
    use std::sync::OnceLock;
    static TABLE: OnceLock<[u32; 256]> = OnceLock::new();
    let table = TABLE.get_or_init(|| {
        let mut t = [0u32; 256];
        let mut n = 0;
        while n < 256 {
            let mut c = n as u32;
            let mut k = 0;
            while k < 8 {
                c = if c & 1 != 0 {
                    0xEDB8_8320 ^ (c >> 1)
                } else {
                    c >> 1
                };
                k += 1;
            }
            t[n] = c;
            n += 1;
        }
        t
    });
    for &b in bytes {
        crc = table[((crc ^ u32::from(b)) & 0xFF) as usize] ^ (crc >> 8);
    }
    crc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replays_last_of_several_records() {
        let mut j = frame(b"first").unwrap();
        j.extend(frame(b"second").unwrap());
        j.extend(frame(b"third").unwrap());
        assert_eq!(replay_bytes(&j).as_deref(), Some(&b"third"[..]));
    }

    #[test]
    fn torn_payload_tail_is_discarded() {
        let mut j = frame(b"good record").unwrap();
        let torn = frame(b"this one was mid-write when the crash hit").unwrap();
        j.extend_from_slice(&torn[..torn.len() - 10]); // drop the last 10 payload bytes
                                                       // The good record survives; the torn tail is ignored, not fatal.
        assert_eq!(replay_bytes(&j).as_deref(), Some(&b"good record"[..]));
    }

    #[test]
    fn torn_header_tail_is_discarded() {
        let mut j = frame(b"complete").unwrap();
        j.extend_from_slice(&[0x03, 0x00]); // 2 stray bytes — an incomplete header
        assert_eq!(replay_bytes(&j).as_deref(), Some(&b"complete"[..]));
    }

    #[test]
    fn corrupt_crc_stops_replay() {
        let mut j = frame(b"ok").unwrap();
        let mut bad = frame(b"payload").unwrap();
        let flip = bad.len() - 1;
        bad[flip] ^= 0xFF; // corrupt the payload so its CRC no longer matches
        j.extend(bad);
        assert_eq!(replay_bytes(&j).as_deref(), Some(&b"ok"[..]));
    }

    #[test]
    fn empty_or_garbage_journal_yields_nothing() {
        assert_eq!(replay_bytes(b""), None);
        assert_eq!(replay_bytes(&[0xFF, 0x00, 0x12]), None); // too short for even a header
    }

    /// #845: zeros left by power loss are not a record, even though crc32 of nothing is 0.
    #[test]
    fn zero_filled_tail_is_not_a_record() {
        let mut j = frame(b"GOOD UNSAVED WORK\n").unwrap();
        j.extend(vec![0u8; 4096]);
        assert_eq!(
            replay_bytes(&j).as_deref(),
            Some(&b"GOOD UNSAVED WORK\n"[..])
        );
        assert_eq!(replay_bytes(&[0u8; 64]), None);
    }

    /// An emptied buffer is still a real record.
    #[test]
    fn empty_payload_is_a_record() {
        let mut j = frame(b"before").unwrap();
        j.extend(frame(b"").unwrap());
        assert_eq!(replay_bytes(&j).as_deref(), Some(&b""[..]));
    }

    /// #844: records appended after a torn tail must be replayed.
    #[test]
    fn append_after_a_torn_tail_is_replayed() {
        let dir = std::env::temp_dir().join(format!("ruse-journal-torn-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let doc = dir.join("f.txt");
        let mut j = frame(b"older recovered\n").unwrap();
        let torn = frame(b"cut off by the crash").unwrap();
        j.extend_from_slice(&torn[..torn.len() - 5]);
        fs::write(journal_path(Some(&doc)), &j).unwrap();
        append(Some(&doc), b"new work\n").unwrap();
        append(Some(&doc), b"newer work\n").unwrap();
        assert_eq!(replay(Some(&doc)).as_deref(), Some(&b"newer work\n"[..]));
        let _ = fs::remove_dir_all(&dir);
    }

    /// #858: however many appends, the journal holds a few frames at most and replays the latest.
    #[test]
    fn journal_stays_bounded_across_many_appends() {
        const BOUND: usize = 8; // "a few snapshots", independent of the tunable constant
        const _: () = assert!(MAX_FRAMES <= BOUND);
        let dir = std::env::temp_dir().join(format!("ruse-journal-bound-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let doc = dir.join("big.txt");
        let payload = vec![b'x'; 4096];
        for i in 0..50u8 {
            let mut p = payload.clone();
            p.push(i);
            append(Some(&doc), &p).unwrap();
            let raw = fs::read(journal_path(Some(&doc))).unwrap();
            assert!(frames(&raw).len() <= BOUND, "append {i}: unbounded");
            assert_eq!(frames(&raw).last().map(|&(_, e)| e), Some(raw.len()));
        }
        let mut last = payload;
        last.push(49);
        assert_eq!(replay(Some(&doc)), Some(last));
        let len = fs::metadata(journal_path(Some(&doc))).unwrap().len() as usize;
        assert!(len <= BOUND * (HEADER + 4097), "journal is {len} bytes");
        clear(Some(&doc));
        let _ = fs::remove_dir_all(&dir);
    }

    /// The crash-path append re-checks the tail even after this process has appended before.
    #[test]
    fn fresh_append_cuts_a_torn_tail_left_mid_process() {
        let dir = std::env::temp_dir().join(format!("ruse-journal-fresh-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let doc = dir.join("f.txt");
        append(Some(&doc), b"one\n").unwrap();
        let torn = frame(b"interrupted by the panic").unwrap();
        let mut f = OpenOptions::new()
            .append(true)
            .open(journal_path(Some(&doc)))
            .unwrap();
        f.write_all(&torn[..torn.len() - 3]).unwrap();
        drop(f);
        append_fresh(Some(&doc), b"rescued\n").unwrap();
        assert_eq!(replay(Some(&doc)).as_deref(), Some(&b"rescued\n"[..]));
        clear(Some(&doc));
        let _ = fs::remove_dir_all(&dir);
    }

    /// #858: idle ticks and cursor motion (same revision) never ask for a frame; edits do, every Nth.
    #[test]
    fn throttle_counts_revision_changes_not_ticks() {
        let mut t = Throttle::default();
        assert_eq!(
            t.tick(5, true, 2),
            Tick::Wait,
            "first sight of a buffer is not a change"
        );
        for _ in 0..100 {
            assert_eq!(
                t.tick(5, true, 2),
                Tick::Wait,
                "an unchanged buffer must not journal"
            );
        }
        assert_eq!(t.tick(6, true, 2), Tick::Wait);
        assert_eq!(t.tick(7, true, 2), Tick::Append, "the 2nd change journals");
        for _ in 0..100 {
            assert_eq!(
                t.tick(7, true, 2),
                Tick::Wait,
                "nothing changed since the last frame"
            );
        }
        assert_eq!(
            t.tick(8, false, 2),
            Tick::Discard,
            "a buffer that became clean drops its journal"
        );
        assert_eq!(t.tick(8, false, 2), Tick::Wait, "only once");
        assert_eq!(t.tick(9, true, 2), Tick::Wait);
        assert_eq!(t.tick(10, true, 2), Tick::Append);
    }

    /// #862: a buffer first seen clean (just opened, its stale journal matching disk) does not delete it.
    #[test]
    fn throttle_never_discards_for_a_buffer_never_seen_modified() {
        let mut t = Throttle::default();
        for r in 0..5 {
            assert_eq!(t.tick(r, false, 2), Tick::Wait);
        }
    }

    /// #873: a payload the `u32` length field cannot describe is refused, not truncated into a frame no
    /// replay can read. Checked through the header seam: a real 4 GiB buffer is too big for a test.
    #[test]
    fn oversized_payload_is_refused_not_truncated() {
        let max = u32::MAX as usize;
        let h = frame_header(max, b"").unwrap();
        assert_eq!(u32::from_le_bytes([h[4], h[5], h[6], h[7]]), u32::MAX);
        #[cfg(target_pointer_width = "64")]
        {
            let e = frame_header(max + 1, b"").unwrap_err();
            assert_eq!(e.kind(), io::ErrorKind::FileTooLarge);
            assert!(e.to_string().contains("too large"), "{e}");
        }
    }

    /// The in-place replay returns the same payload the copying one does, and the streaming CRC matches.
    #[test]
    fn owned_replay_matches_borrowed_replay() {
        let mut j = frame(b"one").unwrap();
        j.extend(frame(b"two two").unwrap());
        j.extend_from_slice(b"RJS0torn");
        assert_eq!(replay_owned(j.clone()), replay_bytes(&j));
        assert_eq!(replay_owned(Vec::new()), None);
        let len = 3u32.to_le_bytes();
        assert_eq!(frame_crc(len, b"abc"), crc32(&[&len[..], b"abc"].concat()));
    }

    /// #869: a FIFO or a device where the journal should be is never opened as one: replay and append
    /// fail at once instead of blocking (FIFO) or reading without end (`/dev/zero`), and discard keeps it.
    #[cfg(unix)]
    #[test]
    fn a_journal_that_is_not_a_regular_file_is_refused_without_blocking() {
        use std::os::unix::ffi::OsStrExt;
        let dir = std::env::temp_dir().join(format!("ruse-journal-fifo-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let doc = dir.join("f.txt");
        let jp = journal_path(Some(&doc));
        let c = std::ffi::CString::new(jp.as_os_str().as_bytes()).unwrap();
        // SAFETY: `c` is a valid NUL-terminated path.
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        let t = std::time::Instant::now();
        let e = replay_checked(Some(&doc)).unwrap_err();
        assert_eq!(e.to_string(), "is not a regular file");
        let e = append(Some(&doc), b"work\n").unwrap_err();
        assert_eq!(e.to_string(), "is not a regular file");
        let e = append_fresh(Some(&doc), b"work\n").unwrap_err();
        assert_eq!(e.to_string(), "is not a regular file");
        discard(Some(&doc));
        assert!(t.elapsed() < std::time::Duration::from_secs(2), "blocked");
        assert!(
            fs::symlink_metadata(&jp).is_ok(),
            "discard leaves a FIFO alone"
        );
        let _ = fs::remove_dir_all(&dir);

        // A journal path that is a symlink to `/dev/zero` is refused before any read.
        let dir = std::env::temp_dir().join(format!("ruse-journal-dev-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let doc = dir.join("f.txt");
        std::os::unix::fs::symlink("/dev/zero", journal_path(Some(&doc))).unwrap();
        let e = replay_checked(Some(&doc)).unwrap_err();
        assert_eq!(e.to_string(), "is not a regular file");
        assert!(append(Some(&doc), b"work\n").is_err());
        let _ = fs::remove_dir_all(&dir);
    }

    /// #862: discard removes a readable journal and keeps one it cannot read.
    #[cfg(unix)]
    #[test]
    fn discard_removes_a_readable_journal_and_keeps_an_unreadable_one() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("ruse-journal-discard-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let doc = dir.join("f.txt");
        append(Some(&doc), b"work\n").unwrap();
        discard(Some(&doc));
        assert!(!journal_path(Some(&doc)).exists());
        discard(Some(&doc)); // no journal: nothing happens
                             // SAFETY: geteuid has no preconditions. Root reads a mode-000 file, so the case does not apply.
        if unsafe { libc::geteuid() } != 0 {
            append(Some(&doc), b"work\n").unwrap();
            let jp = journal_path(Some(&doc));
            fs::set_permissions(&jp, fs::Permissions::from_mode(0o000)).unwrap();
            discard(Some(&doc));
            assert!(jp.exists(), "an unreadable journal is kept");
            fs::set_permissions(&jp, fs::Permissions::from_mode(0o644)).unwrap();
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn crc32_matches_known_vector() {
        // CRC-32/IEEE of "123456789" is 0xCBF43926 (standard check value).
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }

    mod fuzz {
        use super::*;
        use proptest::prelude::*;
        use std::sync::atomic::{AtomicU32, Ordering};

        fn bytes(max: usize) -> impl Strategy<Value = Vec<u8>> {
            proptest::collection::vec(any::<u8>(), 0..max)
        }

        /// A fresh document path per case: [`KNOWN`] caches frame counts by path, so reusing one would
        /// let an earlier case's count skip the torn-tail cut being tested.
        fn fresh_doc(tag: &str) -> (PathBuf, PathBuf) {
            static N: AtomicU32 = AtomicU32::new(0);
            let n = N.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!(
                "ruse-journal-fuzz-{tag}-{}-{n}",
                std::process::id()
            ));
            fs::create_dir_all(&dir).unwrap();
            (dir.join("doc.txt"), dir)
        }

        proptest! {
            #![proptest_config(crate::test_cases(256))]

            /// Arbitrary bytes (and arbitrary bytes behind the magic) never panic replay.
            #[test]
            fn replay_never_panics_on_arbitrary_bytes(raw in bytes(256), tail in bytes(64)) {
                let _ = replay_bytes(&raw);
                let mut j = MAGIC.to_vec();
                j.extend_from_slice(&raw);
                let _ = replay_bytes(&j);
                let mut j = frame(&raw).unwrap();
                j.extend_from_slice(&tail);
                // A valid frame followed by junk replays that frame (or a later valid one the junk forms).
                prop_assert!(replay_bytes(&j).is_some());
            }

            /// Any single corrupted byte in a run of frames replays one of the written payloads or nothing —
            /// never bytes that were not a whole record.
            #[test]
            fn one_corrupt_byte_yields_a_real_record_or_none(
                payloads in proptest::collection::vec(bytes(32), 1..5),
                at in any::<proptest::sample::Index>(),
                xor in 1u8..=255,
            ) {
                let mut j = Vec::new();
                for p in &payloads {
                    j.extend(frame(p).unwrap());
                }
                let i = at.index(j.len());
                j[i] ^= xor;
                if let Some(got) = replay_bytes(&j) {
                    prop_assert!(payloads.contains(&got), "replayed a non-record: {:?}", got);
                }
            }

            /// Whatever garbage a journal file already holds, an append lands a record that replays.
            #[test]
            fn append_after_arbitrary_garbage_replays_the_new_record(
                prefix in proptest::collection::vec(bytes(24), 0..3),
                junk in bytes(64),
                payload in bytes(64),
            ) {
                let (doc, dir) = fresh_doc("append");
                let mut j = Vec::new();
                for p in &prefix {
                    j.extend(frame(p).unwrap());
                }
                j.extend_from_slice(&junk);
                fs::write(journal_path(Some(&doc)), &j).unwrap();
                append(Some(&doc), &payload).unwrap();
                let got = replay(Some(&doc));
                clear(Some(&doc));
                let _ = fs::remove_dir_all(&dir);
                prop_assert_eq!(got, Some(payload));
            }
        }
    }

    /// #907: a journal append that cannot write (here: the directory is read-only) is an error the caller
    /// sees, not a panic and not a silent success; a previously written journal is left as it was.
    #[cfg(unix)]
    #[test]
    fn append_into_a_read_only_directory_fails_visibly() {
        use std::os::unix::fs::PermissionsExt;
        // SAFETY: getuid has no preconditions.
        if unsafe { libc::getuid() } == 0 {
            return; // root ignores directory permissions
        }
        let dir = std::env::temp_dir().join(format!("ruse-journal-rodir-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let doc = dir.join("f.txt");
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o555)).unwrap();
        let err = append(Some(&doc), b"unsaved\n");
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(err.is_err(), "append into a read-only dir must fail");
        assert_eq!(replay(Some(&doc)), None);

        // An existing journal made read-only keeps its last record when an append is refused.
        append(Some(&doc), b"older\n").unwrap();
        let jp = journal_path(Some(&doc));
        fs::set_permissions(&jp, fs::Permissions::from_mode(0o444)).unwrap();
        let err = append_fresh(Some(&doc), b"newer\n");
        fs::set_permissions(&jp, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(err.is_err(), "append to a read-only journal must fail");
        assert_eq!(replay(Some(&doc)).as_deref(), Some(&b"older\n"[..]));
        let _ = fs::remove_dir_all(&dir);
    }
}
