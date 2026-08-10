//! Append-only recovery journal (F-008 acceptance #4: a truncated tail is discarded, not fatal).
//!
//! Unsaved work is captured as length-and-CRC framed records appended to `<file>.ruse-journal`.
//! Recovery [`replay`]s the frames and returns the LAST fully-valid one; a torn final write (partial
//! header, partial payload, or a bad CRC — exactly what a crash mid-append leaves) is discarded and
//! the prior valid record stands. A durable save deletes the journal (the work is now on disk).
//!
//! Frame: `[u32 len LE][u32 crc32 LE][payload]`, CRC over the payload. This is the MVP-minimal
//! journal — whole-buffer snapshots, throttled by the caller. The design doc's per-transaction
//! incremental records + schema version + inverse-edits are the post-MVP C-PERSIST elaboration; the
//! frame layout here is forward-compatible (a versioned header can prefix the file later).

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

const HEADER: usize = 8; // u32 len + u32 crc

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
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(&crc32(payload).to_le_bytes());
    out.extend_from_slice(payload);
    out
}

/// Append a recovery record. Creates the journal if absent; never truncates existing records.
pub fn append(doc: Option<&Path>, payload: &[u8]) -> io::Result<()> {
    let mut f = OpenOptions::new()
        .create(true)
        .append(true)
        .open(journal_path(doc))?;
    f.write_all(&frame(payload))?;
    f.sync_all()
}

/// Remove the journal — called after a durable save, when there is no unsaved work to recover.
pub fn clear(doc: Option<&Path>) {
    let _ = fs::remove_file(journal_path(doc));
}

/// The payload of the last VALID frame in a raw journal, or `None` if there is no intact record.
/// Stops at the first torn/corrupt frame, so a crash mid-append costs at most the final record.
pub fn replay_bytes(raw: &[u8]) -> Option<Vec<u8>> {
    let mut last: Option<Vec<u8>> = None;
    let mut i = 0;
    while i + HEADER <= raw.len() {
        let len = u32::from_le_bytes([raw[i], raw[i + 1], raw[i + 2], raw[i + 3]]) as usize;
        let want_crc = u32::from_le_bytes([raw[i + 4], raw[i + 5], raw[i + 6], raw[i + 7]]);
        let start = i + HEADER;
        let end = match start.checked_add(len) {
            Some(e) if e <= raw.len() => e,
            _ => break, // payload runs past EOF — torn tail
        };
        let payload = &raw[start..end];
        if crc32(payload) != want_crc {
            break; // corrupt frame — torn tail
        }
        last = Some(payload.to_vec());
        i = end;
    }
    last
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
        j.extend_from_slice(&[0x03, 0x00]); // 2 stray bytes — an incomplete 8-byte header
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

    #[test]
    fn crc32_matches_known_vector() {
        // CRC-32/IEEE of "123456789" is 0xCBF43926 (standard check value).
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }
}
