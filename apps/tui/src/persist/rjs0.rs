//! The legacy `RJS0` snapshot journal, read-only (RFC-0023 §3 migration, D-064).
//!
//! Before RJ01, unsaved work was a run of whole-buffer snapshots in a sidecar `<file>.ruse-journal`, each
//! framed `[magic "RJS0"][u32 len LE][u32 crc32 LE][payload]` with a CRC-32/IEEE over the length bytes and
//! the payload. A sidecar left by an older ruse is read once: its last intact snapshot is offered like an
//! RJ01 journal, and the file is then moved to the journal archive. This build never writes one.

use std::path::{Path, PathBuf};

const MAGIC: [u8; 4] = *b"RJS0";
const HEADER: usize = 12;

/// Where an older ruse kept `doc`'s journal: `<file>.ruse-journal` next to it, or `.ruse-journal` in the
/// working directory for a buffer with no file.
pub fn sidecar_path(doc: Option<&Path>) -> PathBuf {
    match doc {
        Some(p) => {
            let mut name = p.file_name().unwrap_or_default().to_os_string();
            name.push(".ruse-journal");
            p.with_file_name(name)
        }
        None => PathBuf::from(".ruse-journal"),
    }
}

/// The payload of the last intact frame of a raw RJS0 journal; a torn or corrupt frame and everything
/// after it are ignored.
pub fn last_snapshot(raw: &[u8]) -> Option<&[u8]> {
    let mut last = None;
    let mut i = 0;
    while i + HEADER <= raw.len() {
        if raw[i..i + 4] != MAGIC {
            break;
        }
        let len = [raw[i + 4], raw[i + 5], raw[i + 6], raw[i + 7]];
        let want = u32::from_le_bytes([raw[i + 8], raw[i + 9], raw[i + 10], raw[i + 11]]);
        let start = i + HEADER;
        let end = match start.checked_add(u32::from_le_bytes(len) as usize) {
            Some(e) if e <= raw.len() => e,
            _ => break,
        };
        if crc32_finish(crc32_update(crc32_update(CRC_INIT, &len), &raw[start..end])) != want {
            break;
        }
        last = Some(&raw[start..end]);
        i = end;
    }
    last
}

const CRC_INIT: u32 = 0xFFFF_FFFF;

fn crc32_finish(crc: u32) -> u32 {
    crc ^ 0xFFFF_FFFF
}

/// CRC-32/IEEE 802.3, the checksum RJS0 frames carry.
fn crc32_update(mut crc: u32, bytes: &[u8]) -> u32 {
    use std::sync::OnceLock;
    static TABLE: OnceLock<[u32; 256]> = OnceLock::new();
    let table = TABLE.get_or_init(|| {
        let mut t = [0u32; 256];
        for (n, slot) in t.iter_mut().enumerate() {
            let mut c = n as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 {
                    0xEDB8_8320 ^ (c >> 1)
                } else {
                    c >> 1
                };
            }
            *slot = c;
        }
        t
    });
    for &b in bytes {
        crc = table[((crc ^ u32::from(b)) & 0xFF) as usize] ^ (crc >> 8);
    }
    crc
}

/// One RJS0 frame, as an older ruse wrote it. Only tests write the legacy format.
#[doc(hidden)]
pub fn frame(payload: &[u8]) -> Vec<u8> {
    let len = u32::try_from(payload.len())
        .expect("a test frame fits")
        .to_le_bytes();
    let crc = crc32_finish(crc32_update(crc32_update(CRC_INIT, &len), payload));
    let mut out = MAGIC.to_vec();
    out.extend_from_slice(&len);
    out.extend_from_slice(&crc.to_le_bytes());
    out.extend_from_slice(payload);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc32_matches_the_check_value() {
        assert_eq!(
            crc32_finish(crc32_update(CRC_INIT, b"123456789")),
            0xCBF4_3926
        );
    }

    #[test]
    fn the_last_intact_snapshot_is_read() {
        let mut j = frame(b"first");
        j.extend(frame(b"second"));
        assert_eq!(last_snapshot(&j), Some(&b"second"[..]));
        let torn = frame(b"third, cut short");
        j.extend_from_slice(&torn[..torn.len() - 4]);
        assert_eq!(last_snapshot(&j), Some(&b"second"[..]));
        j.extend(vec![0u8; 64]);
        assert_eq!(last_snapshot(&j), Some(&b"second"[..]));
        assert_eq!(last_snapshot(b""), None);
        assert_eq!(last_snapshot(&[0u8; 64]), None);
        assert_eq!(last_snapshot(&frame(b"")), Some(&b""[..]));
    }

    #[test]
    fn a_corrupt_frame_stops_the_read() {
        let mut j = frame(b"ok");
        let mut bad = frame(b"payload");
        let n = bad.len() - 1;
        bad[n] ^= 0xFF;
        j.extend(bad);
        assert_eq!(last_snapshot(&j), Some(&b"ok"[..]));
    }

    #[test]
    fn sidecar_names_match_the_old_layout() {
        assert_eq!(
            sidecar_path(Some(Path::new("/d/notes.md"))),
            Path::new("/d/notes.md.ruse-journal")
        );
        assert_eq!(sidecar_path(None), Path::new(".ruse-journal"));
    }
}
