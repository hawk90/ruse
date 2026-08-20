//! Kitty graphics-protocol lowering for inline images (F-031 slice 3b). PURE and terminal-free: it builds
//! the `APC _G` escape byte strings and reconciles on-screen placements against a frame; the render loop
//! (slice 3b-2b) does the file IO and writes the bytes. No `image` crate — Kitty decodes PNG itself
//! (`f=100`), so we hand it the file's raw bytes. See docs/design/rich-rendering.md §8.3-8.7.

use std::collections::{HashMap, HashSet};

/// Base64 (standard alphabet, padded) — the encoding Kitty payloads use. Hand-rolled to avoid a dep.
pub fn base64(data: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(A[(n >> 18 & 63) as usize] as char);
        out.push(A[(n >> 12 & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            A[(n >> 6 & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            A[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

/// A resident image's id (stable per source — e.g. a hash of its path + mtime; assigned by the caller).
pub type ImageId = u32;

/// One placement identifier per image in this slice (a single on-screen copy).
const PLACEMENT_ID: u32 = 1;

/// The base64 chunk size Kitty accepts per `APC` (payload is split into `m=1` continuations).
const CHUNK: usize = 4096;

/// Where an image sits on screen, in CELLS — the unit the placement diff compares.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Placement {
    pub row: u16,
    pub col: u16,
    pub cols: u16,
    pub rows: u16,
}

/// The `APC _G a=t` TRANSMIT command for a PNG image: `f=100` (PNG), `i=<id>`, base64 payload chunked at
/// [`CHUNK`] bytes with `m=1` on every chunk but the last. Kitty decodes the PNG itself.
pub fn transmit_png(id: ImageId, png: &[u8]) -> Vec<u8> {
    let payload = base64(png);
    let bytes = payload.as_bytes();
    let mut out = Vec::new();
    if bytes.is_empty() {
        return out;
    }
    let mut chunks = bytes.chunks(CHUNK).peekable();
    let mut first = true;
    while let Some(chunk) = chunks.next() {
        let more = if chunks.peek().is_some() { 1 } else { 0 };
        out.extend_from_slice(b"\x1b_G");
        if first {
            // Only the FIRST chunk carries the control keys; continuations carry `m` alone.
            out.extend_from_slice(format!("a=t,f=100,i={id},m={more}").as_bytes());
            first = false;
        } else {
            out.extend_from_slice(format!("m={more}").as_bytes());
        }
        out.push(b';');
        out.extend_from_slice(chunk);
        out.extend_from_slice(b"\x1b\\");
    }
    out
}

/// The `a=p` PLACE command — display image `id` in a `cols × rows` cell box. The caller moves the cursor
/// to the target `(row, col)` first (a `CSI <row>;<col> H`), which [`move_cursor`] builds.
pub fn place(id: ImageId, cols: u16, rows: u16) -> Vec<u8> {
    format!("\x1b_Ga=p,i={id},p={PLACEMENT_ID},c={cols},r={rows}\x1b\\").into_bytes()
}

/// A `CSI <row>;<col> H` cursor move (1-based), for positioning a placement.
pub fn move_cursor(row: u16, col: u16) -> Vec<u8> {
    format!("\x1b[{};{}H", row + 1, col + 1).into_bytes()
}

/// The `a=d,d=i` DELETE-placement command — removes the on-screen copy but keeps the transmitted image.
pub fn delete_placement(id: ImageId) -> Vec<u8> {
    format!("\x1b_Ga=d,d=i,i={id},p={PLACEMENT_ID}\x1b\\").into_bytes()
}

/// The `a=d,d=I` FREE-image command — releases the transmitted image (on cache eviction).
pub fn free_image(id: ImageId) -> Vec<u8> {
    format!("\x1b_Ga=d,d=I,i={id}\x1b\\").into_bytes()
}

/// Parse a PNG's pixel dimensions from its IHDR (`(width, height)`), or `None` if the bytes are not a PNG.
/// The 8-byte signature is followed by the IHDR chunk whose data starts at byte 16: width then height, big-
/// endian u32. Kitty needs no client decode, but we read the size to choose the block's cell height.
pub fn png_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    const SIG: &[u8] = &[0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n'];
    if bytes.len() < 24 || &bytes[..8] != SIG || &bytes[12..16] != b"IHDR" {
        return None;
    }
    let w = u32::from_be_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]);
    let h = u32::from_be_bytes([bytes[20], bytes[21], bytes[22], bytes[23]]);
    (w > 0 && h > 0).then_some((w, h))
}

/// The cell height (rows) to show an `img_w × img_h`-px image across `cols` cells, preserving aspect.
/// `cell_aspect` = cell width ÷ cell height (≈ 0.5 for a typical monospace cell). Clamped to `[1, max_rows]`.
pub fn fit_rows(img_w: u32, img_h: u32, cols: u16, cell_aspect: f32, max_rows: u16) -> u16 {
    if img_w == 0 || cols == 0 {
        return 1;
    }
    let rows = (f32::from(cols) * (img_h as f32 / img_w as f32) * cell_aspect).round();
    (rows as u16).clamp(1, max_rows)
}

/// One reconciliation op the render loop turns into bytes (reading the file only for `Transmit`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum GraphicsOp {
    /// Transmit the (not-yet-resident) image before its first placement.
    Transmit(ImageId),
    /// Place / re-place an image at a new cell rectangle.
    Place { id: ImageId, at: Placement },
    /// Remove an on-screen placement no longer visible (the image stays resident).
    DeletePlacement(ImageId),
}

/// Reconcile the previous placements against what is visible NOW (§8.3): transmit newly-resident images,
/// (re)place new or moved ones, delete those scrolled off, and emit nothing for an unchanged placement —
/// mirroring the cell diff's still-frame silence. `placed` and `resident` are updated to the new state.
pub fn reconcile(
    placed: &mut HashMap<ImageId, Placement>,
    resident: &mut HashSet<ImageId>,
    visible: &[(ImageId, Placement)],
) -> Vec<GraphicsOp> {
    let now: HashMap<ImageId, Placement> = visible.iter().copied().collect();
    let mut ops = Vec::new();
    // Deletes first: previously placed, not visible now.
    for &id in placed.keys() {
        if !now.contains_key(&id) {
            ops.push(GraphicsOp::DeletePlacement(id));
        }
    }
    // Transmit (once) + place (new or moved). Unchanged placements emit nothing.
    for &(id, at) in visible {
        if resident.insert(id) {
            ops.push(GraphicsOp::Transmit(id));
        }
        if placed.get(&id) != Some(&at) {
            ops.push(GraphicsOp::Place { id, at });
        }
    }
    *placed = now;
    ops
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_known_vectors() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn transmit_wraps_apc_and_marks_continuations() {
        // A payload larger than one chunk splits into m=1 … m=0 across multiple APCs.
        let big = vec![0u8; CHUNK * 2]; // base64 is longer than the raw, so ≥3 chunks
        let out = transmit_png(7, &big);
        let s = String::from_utf8_lossy(&out);
        assert!(
            s.starts_with("\x1b_Ga=t,f=100,i=7,m=1;"),
            "first chunk carries the keys"
        );
        assert!(s.contains("\x1b_Gm=0;"), "the last chunk is marked m=0");
        assert!(s.ends_with("\x1b\\"), "each chunk is APC-terminated");
        assert!(!out.is_empty());
    }

    #[test]
    fn place_delete_free_commands() {
        assert_eq!(place(5, 20, 8), b"\x1b_Ga=p,i=5,p=1,c=20,r=8\x1b\\");
        assert_eq!(delete_placement(5), b"\x1b_Ga=d,d=i,i=5,p=1\x1b\\");
        assert_eq!(free_image(5), b"\x1b_Ga=d,d=I,i=5\x1b\\");
        assert_eq!(move_cursor(2, 4), b"\x1b[3;5H"); // 0-based -> 1-based CSI
    }

    #[test]
    fn png_dimensions_reads_ihdr_and_rejects_non_png() {
        // Minimal PNG header: signature + IHDR length + "IHDR" + 4x4 dimensions.
        let mut png = vec![0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n'];
        png.extend_from_slice(&[0, 0, 0, 13]); // IHDR length
        png.extend_from_slice(b"IHDR");
        png.extend_from_slice(&64u32.to_be_bytes()); // width
        png.extend_from_slice(&48u32.to_be_bytes()); // height
        assert_eq!(png_dimensions(&png), Some((64, 48)));
        assert_eq!(png_dimensions(b"not a png at all really"), None);
    }

    #[test]
    fn fit_rows_preserves_aspect_and_clamps() {
        // A 100x100 square across 10 cells, cells twice as tall as wide (aspect 0.5) -> 5 rows.
        assert_eq!(fit_rows(100, 100, 10, 0.5, 40), 5);
        // A wide banner is at least 1 row.
        assert_eq!(fit_rows(1000, 10, 10, 0.5, 40), 1);
        // Clamped to max_rows.
        assert_eq!(fit_rows(10, 1000, 10, 0.5, 40), 40);
    }

    #[test]
    fn reconcile_transmits_places_moves_and_deletes() {
        let mut placed = HashMap::new();
        let mut resident = HashSet::new();
        let a = Placement {
            row: 1,
            col: 0,
            cols: 20,
            rows: 5,
        };
        // First frame: image 1 newly visible -> transmit + place.
        let ops = reconcile(&mut placed, &mut resident, &[(1, a)]);
        assert_eq!(
            ops,
            vec![GraphicsOp::Transmit(1), GraphicsOp::Place { id: 1, at: a }]
        );
        // Same position next frame -> nothing (still-frame silence; already resident).
        assert!(reconcile(&mut placed, &mut resident, &[(1, a)]).is_empty());
        // Moved -> re-place only (no re-transmit).
        let b = Placement { row: 3, ..a };
        assert_eq!(
            reconcile(&mut placed, &mut resident, &[(1, b)]),
            vec![GraphicsOp::Place { id: 1, at: b }],
        );
        // Scrolled off -> delete the placement.
        assert_eq!(
            reconcile(&mut placed, &mut resident, &[]),
            vec![GraphicsOp::DeletePlacement(1)],
        );
    }
}
