//! Append-only recovery journal (F-008 acceptance #4: a truncated tail is discarded, not fatal).
//!
//! Unsaved work is captured as length-and-CRC framed records appended to `<file>.ruse-journal`.
//! Recovery [`replay`]s the frames and returns the LAST fully-valid one; a torn final write (partial
//! header, partial payload, or a bad CRC — exactly what a crash mid-append leaves) is discarded and
//! the prior valid record stands. A durable save deletes the journal (the work is now on disk).
//!
//! Frame: `[magic "RJS0"][u32 len LE][u32 crc32 LE][payload]`, CRC over the length bytes and the payload,
//! so a zero-filled tail (a known outcome of power loss) is never a valid record. Before the first append in
//! a process, a torn tail is cut off, and a failed append is rolled back, so new records never land behind
//! bytes that replay stops at. Journals written before the magic existed are not read. This is the MVP-minimal
//! journal — whole-buffer snapshots, throttled by the caller. The design doc's per-transaction
//! incremental records + schema version + inverse-edits are the post-MVP C-PERSIST elaboration; the
//! frame layout here is forward-compatible (a versioned header can prefix the file later).

use std::collections::HashSet;
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

/// Encode one framed record.
pub fn frame(payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER + payload.len());
    let len = (payload.len() as u32).to_le_bytes();
    out.extend_from_slice(&MAGIC);
    out.extend_from_slice(&len);
    out.extend_from_slice(&frame_crc(len, payload).to_le_bytes());
    out.extend_from_slice(payload);
    out
}

fn frame_crc(len: [u8; 4], payload: &[u8]) -> u32 {
    let mut bytes = Vec::with_capacity(4 + payload.len());
    bytes.extend_from_slice(&len);
    bytes.extend_from_slice(payload);
    crc32(&bytes)
}

/// Append a recovery record. Creates the journal if absent and never drops a valid record. The first
/// append to a journal in this process cuts off any torn tail first; a failed append is rolled back.
pub fn append(doc: Option<&Path>, payload: &[u8]) -> io::Result<()> {
    static CHECKED: Mutex<Option<HashSet<PathBuf>>> = Mutex::new(None);
    let path = journal_path(doc);
    let mut f = OpenOptions::new()
        .create(true)
        .read(true)
        .append(true)
        .open(&path)?;
    let first = CHECKED
        .lock()
        .map(|mut set| set.get_or_insert_with(HashSet::new).insert(path.clone()))
        .unwrap_or(true);
    if first {
        let raw = fs::read(&path)?;
        let valid = valid_prefix(&raw);
        if valid < raw.len() {
            f.set_len(valid as u64)?;
        }
    }
    let before = f.metadata()?.len();
    let written = f.write_all(&frame(payload)).and_then(|()| f.sync_all());
    if written.is_err() {
        let _ = f.set_len(before);
    }
    written
}

/// Remove the journal — called after a durable save, when there is no unsaved work to recover.
pub fn clear(doc: Option<&Path>) {
    let _ = fs::remove_file(journal_path(doc));
}

/// The payload of the last VALID frame in a raw journal, or `None` if there is no intact record.
/// Stops at the first torn/corrupt frame, so a crash mid-append costs at most the final record.
pub fn replay_bytes(raw: &[u8]) -> Option<Vec<u8>> {
    frames(raw)
        .last()
        .map(|&(start, end)| raw[start..end].to_vec())
}

/// The length of the leading run of valid frames; everything after it is a torn or corrupt tail.
fn valid_prefix(raw: &[u8]) -> usize {
    frames(raw).last().map_or(0, |&(_, end)| end)
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
    let raw = fs::read(journal_path(doc)).ok()?;
    replay_bytes(&raw)
}

/// Standard CRC-32 (IEEE 802.3), table computed once. No external crate; correctness over speed
/// (perf is the post-MVP pass).
fn crc32(bytes: &[u8]) -> u32 {
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
    let mut crc = 0xFFFF_FFFFu32;
    for &b in bytes {
        crc = table[((crc ^ u32::from(b)) & 0xFF) as usize] ^ (crc >> 8);
    }
    crc ^ 0xFFFF_FFFF
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replays_last_of_several_records() {
        let mut j = frame(b"first");
        j.extend(frame(b"second"));
        j.extend(frame(b"third"));
        assert_eq!(replay_bytes(&j).as_deref(), Some(&b"third"[..]));
    }

    #[test]
    fn torn_payload_tail_is_discarded() {
        let mut j = frame(b"good record");
        let torn = frame(b"this one was mid-write when the crash hit");
        j.extend_from_slice(&torn[..torn.len() - 10]); // drop the last 10 payload bytes
                                                       // The good record survives; the torn tail is ignored, not fatal.
        assert_eq!(replay_bytes(&j).as_deref(), Some(&b"good record"[..]));
    }

    #[test]
    fn torn_header_tail_is_discarded() {
        let mut j = frame(b"complete");
        j.extend_from_slice(&[0x03, 0x00]); // 2 stray bytes — an incomplete header
        assert_eq!(replay_bytes(&j).as_deref(), Some(&b"complete"[..]));
    }

    #[test]
    fn corrupt_crc_stops_replay() {
        let mut j = frame(b"ok");
        let mut bad = frame(b"payload");
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
        let mut j = frame(b"GOOD UNSAVED WORK\n");
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
        let mut j = frame(b"before");
        j.extend(frame(b""));
        assert_eq!(replay_bytes(&j).as_deref(), Some(&b""[..]));
    }

    /// #844: records appended after a torn tail must be replayed.
    #[test]
    fn append_after_a_torn_tail_is_replayed() {
        let dir = std::env::temp_dir().join(format!("ruse-journal-torn-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let doc = dir.join("f.txt");
        let mut j = frame(b"older recovered\n");
        let torn = frame(b"cut off by the crash");
        j.extend_from_slice(&torn[..torn.len() - 5]);
        fs::write(journal_path(Some(&doc)), &j).unwrap();
        append(Some(&doc), b"new work\n").unwrap();
        append(Some(&doc), b"newer work\n").unwrap();
        assert_eq!(replay(Some(&doc)).as_deref(), Some(&b"newer work\n"[..]));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn crc32_matches_known_vector() {
        // CRC-32/IEEE of "123456789" is 0xCBF43926 (standard check value).
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }
}
