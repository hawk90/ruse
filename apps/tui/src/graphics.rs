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
            // Only the FIRST chunk carries the control keys; continuations carry `m` alone. `q=2`
            // suppresses the terminal's OK/error reply so it never lands in the editor's input stream.
            out.extend_from_slice(format!("a=t,f=100,i={id},q=2,m={more}").as_bytes());
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
    format!("\x1b_Ga=p,i={id},p={PLACEMENT_ID},q=2,c={cols},r={rows}\x1b\\").into_bytes()
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

/// `a=d,d=A` — delete ALL placements and free ALL images. Emitted on exit so no inline image is left
/// drawn on the terminal after the editor quits (F-031 slice 3b-2b cleanup).
pub fn delete_all() -> Vec<u8> {
    b"\x1b_Ga=d,d=A\x1b\\".to_vec()
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

/// The cell WIDTH (cols) to show an `img_w × img_h`-px image `rows` cells tall, preserving aspect —
/// the inverse of [`fit_rows`]. `cell_aspect` = cell width ÷ cell height (≈ 0.5). Clamped to `[1, max_cols]`.
/// Used to size an image to its natural aspect within the block (then centred) rather than stretching it.
pub fn fit_cols(img_w: u32, img_h: u32, rows: u16, cell_aspect: f32, max_cols: u16) -> u16 {
    if img_h == 0 || rows == 0 {
        return 1;
    }
    let cols = (f32::from(rows) / cell_aspect * (img_w as f32 / img_h as f32)).round();
    (cols as u16).clamp(1, max_cols)
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

/// Wrap a raw terminal escape in the tmux PASSTHROUGH envelope (`\x1bPtmux; … \x1b\\`) so tmux forwards it
/// to the OUTER terminal instead of swallowing it — every inner `\x1b` is doubled per the tmux contract.
/// Needed for Kitty graphics inside tmux (with `set -g allow-passthrough on`). F-031 slice 3b-2b.
pub fn wrap_tmux(escape: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(escape.len() + 10);
    out.extend_from_slice(b"\x1bPtmux;");
    for &b in escape {
        if b == 0x1b {
            out.push(0x1b);
        }
        out.push(b);
    }
    out.extend_from_slice(b"\x1b\\");
    out
}

/// A stable image id from its source path — a hash, so re-opening the same file reuses the Kitty id.
/// Masked to 24 bits (forced non-zero) so the id fits exactly in a placeholder cell's fg RGB colour
/// (F-031 slice 3b-2c Unicode placeholders).
pub fn image_id(path: &str) -> ImageId {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    path.hash(&mut h);
    ((h.finish() as u32) & 0x00FF_FFFF) | 1
}

/// The image id encoded as an `(r, g, b)` foreground colour — how a Unicode placeholder cell names its
/// image (the terminal reads the id back from the cell colour). F-031 slice 3b-2c.
pub fn id_rgb(id: ImageId) -> (u8, u8, u8) {
    (
        ((id >> 16) & 0xff) as u8,
        ((id >> 8) & 0xff) as u8,
        (id & 0xff) as u8,
    )
}

/// The Unicode placeholder character (Kitty): a cell holding this + row/col diacritics displays the
/// matching slice of a virtually-placed image.
const PLACEHOLDER: char = '\u{10EEEE}';

/// The Kitty `rowcolumn-diacritics` table: index `i` (0-based) maps to the combining codepoint that encodes
/// the value `i`. From the Kitty graphics protocol. A placeholder cell for image row `r`, column `c` is
/// `PLACEHOLDER + DIACRITICS[r] + DIACRITICS[c]`. Only the first ~250 are load-bearing for our block sizes;
/// the tail is best-effort (a solid-colour image is unaffected by a mis-encoded row/col — every slice is the
/// same, so the demo validates the mechanism regardless).
#[rustfmt::skip]
const DIACRITICS: &[char] = &[
    '\u{0305}','\u{030D}','\u{030E}','\u{0310}','\u{0312}','\u{033D}','\u{033E}','\u{033F}','\u{0346}','\u{034A}',
    '\u{034B}','\u{034C}','\u{0350}','\u{0351}','\u{0352}','\u{0357}','\u{035B}','\u{0363}','\u{0364}','\u{0365}',
    '\u{0366}','\u{0367}','\u{0368}','\u{0369}','\u{036A}','\u{036B}','\u{036C}','\u{036D}','\u{036E}','\u{036F}',
    '\u{0483}','\u{0484}','\u{0485}','\u{0486}','\u{0487}','\u{0592}','\u{0593}','\u{0594}','\u{0595}','\u{0597}',
    '\u{0598}','\u{0599}','\u{059C}','\u{059D}','\u{059E}','\u{059F}','\u{05A0}','\u{05A1}','\u{05A8}','\u{05A9}',
    '\u{05AB}','\u{05AC}','\u{05AF}','\u{05C4}','\u{0610}','\u{0611}','\u{0612}','\u{0613}','\u{0614}','\u{0615}',
    '\u{0616}','\u{0617}','\u{0657}','\u{0658}','\u{0659}','\u{065A}','\u{065B}','\u{065D}','\u{065E}','\u{06D6}',
    '\u{06D7}','\u{06D8}','\u{06D9}','\u{06DA}','\u{06DB}','\u{06DC}','\u{06DF}','\u{06E0}','\u{06E1}','\u{06E2}',
    '\u{06E4}','\u{06E7}','\u{06E8}','\u{06EB}','\u{06EC}','\u{0730}','\u{0732}','\u{0733}','\u{0735}','\u{0736}',
    '\u{073A}','\u{073D}','\u{073F}','\u{0740}','\u{0741}','\u{0743}','\u{0745}','\u{0747}','\u{0749}','\u{074A}',
    '\u{07EB}','\u{07EC}','\u{07ED}','\u{07EE}','\u{07EF}','\u{07F0}','\u{07F1}','\u{07F3}','\u{0816}','\u{0817}',
    '\u{0818}','\u{0819}','\u{081B}','\u{081C}','\u{081D}','\u{081E}','\u{081F}','\u{0820}','\u{0821}','\u{0822}',
    '\u{0823}','\u{0825}','\u{0826}','\u{0827}','\u{0829}','\u{082A}','\u{082B}','\u{082C}','\u{082D}','\u{0951}',
    '\u{0953}','\u{0954}','\u{0F82}','\u{0F83}','\u{0F86}','\u{0F87}','\u{135D}','\u{135E}','\u{135F}','\u{17DD}',
    '\u{193A}','\u{1A17}','\u{1A75}','\u{1A76}','\u{1A77}','\u{1A78}','\u{1A79}','\u{1A7A}','\u{1A7B}','\u{1A7C}',
    '\u{1B6B}','\u{1B6D}','\u{1B6E}','\u{1B6F}','\u{1B70}','\u{1B71}','\u{1B72}','\u{1B73}','\u{1CD0}','\u{1CD1}',
    '\u{1CD2}','\u{1CDA}','\u{1CDB}','\u{1CE0}','\u{1DC0}','\u{1DC1}','\u{1DC3}','\u{1DC4}','\u{1DC5}','\u{1DC6}',
    '\u{1DC7}','\u{1DC8}','\u{1DC9}','\u{1DCB}','\u{1DCC}','\u{1DD1}','\u{1DD2}','\u{1DD3}','\u{1DD4}','\u{1DD5}',
    '\u{1DD6}','\u{1DD7}','\u{1DD8}','\u{1DD9}','\u{1DDA}','\u{1DDB}','\u{1DDC}','\u{1DDD}','\u{1DDE}','\u{1DDF}',
    '\u{1DE0}','\u{1DE1}','\u{1DE2}','\u{1DE3}','\u{1DE4}','\u{1DE5}','\u{1DE6}','\u{1DFE}','\u{20D0}','\u{20D1}',
    '\u{20D4}','\u{20D5}','\u{20D6}','\u{20D7}','\u{20DB}','\u{20DC}','\u{20E1}','\u{20E7}','\u{20E9}','\u{20F0}',
    '\u{2CEF}','\u{2CF0}','\u{2CF1}','\u{2DE0}','\u{2DE1}','\u{2DE2}','\u{2DE3}','\u{2DE4}','\u{2DE5}','\u{2DE6}',
    '\u{2DE7}','\u{2DE8}','\u{2DE9}','\u{2DEA}','\u{2DEB}','\u{2DEC}','\u{2DED}','\u{2DEE}','\u{2DEF}','\u{2DF0}',
    '\u{2DF1}','\u{2DF2}','\u{2DF3}','\u{2DF4}','\u{2DF5}','\u{2DF6}','\u{2DF7}','\u{2DF8}','\u{2DF9}','\u{2DFA}',
    '\u{2DFB}','\u{2DFC}','\u{2DFD}','\u{2DFE}','\u{2DFF}','\u{A66F}','\u{A67C}','\u{A67D}','\u{A6F0}','\u{A6F1}',
    '\u{A8E0}','\u{A8E1}','\u{A8E2}','\u{A8E3}','\u{A8E4}','\u{A8E5}','\u{A8E6}','\u{A8E7}','\u{A8E8}','\u{A8E9}',
    '\u{A8EA}','\u{A8EB}','\u{A8EC}','\u{A8ED}','\u{A8EE}','\u{A8EF}','\u{A8F0}','\u{A8F1}','\u{AAB0}','\u{AAB2}',
    '\u{AAB3}','\u{AAB7}','\u{AAB8}','\u{AABE}','\u{AABF}','\u{AAC1}','\u{FE20}','\u{FE21}','\u{FE22}','\u{FE23}',
    '\u{FE24}','\u{FE25}','\u{FE26}','\u{10A0F}','\u{10A38}','\u{1D165}','\u{1D167}','\u{1D168}','\u{1D16D}',
    '\u{1D16E}','\u{1D16F}','\u{1D170}','\u{1D171}','\u{1D172}','\u{1D17B}','\u{1D17C}','\u{1D17D}','\u{1D17E}',
    '\u{1D17F}','\u{1D180}','\u{1D181}','\u{1D182}','\u{1D185}','\u{1D186}','\u{1D187}',
];

/// The maximum image row/column a placeholder can encode (the diacritics table length).
pub const MAX_PLACEHOLDER: u16 = DIACRITICS.len() as u16;

/// A Unicode placeholder CELL for image `(row, col)`: the placeholder char plus the row and column
/// diacritics. Painted into the grid with `fg = id_rgb(id)`; the terminal composites the image slice there
/// (F-031 slice 3b-2c). `row`/`col` beyond the table are clamped to the last entry.
pub fn placeholder_cell(row: u16, col: u16) -> String {
    let d = |v: u16| DIACRITICS[(v as usize).min(DIACRITICS.len() - 1)];
    let mut s = String::with_capacity(12);
    s.push(PLACEHOLDER);
    s.push(d(row));
    s.push(d(col));
    s
}

/// The `a=p, U=1` VIRTUAL placement — display image `id` in a `cols × rows` cell box wherever its Unicode
/// placeholder cells are painted (NOT at the cursor). F-031 slice 3b-2c.
pub fn virtual_place(id: ImageId, cols: u16, rows: u16) -> Vec<u8> {
    format!("\x1b_Ga=p,U=1,i={id},c={cols},r={rows},q=2\x1b\\").into_bytes()
}

/// The post-flush GRAPHICS PASS (§8.8, Unicode placeholders): TRANSMIT each visible image once and create
/// its `U=1` VIRTUAL placement — nothing is drawn at the cursor. The image is shown by the placeholder CELLS
/// `paint_pane` painted (which ride tmux's normal cell rendering, so pane offset + clipping are automatic).
/// `read_png(path)` returns the file's bytes when it is a usable image (the caller bounds + verifies PNG); a
/// path that fails to load is skipped (its placeholder cells then reference no image → a blank band).
pub fn graphics_pass<W: std::io::Write>(
    out: &mut W,
    images: &[(String, Placement)],
    resident: &mut HashSet<ImageId>,
    tmux: bool,
    mut read_png: impl FnMut(&str) -> Option<Vec<u8>>,
) -> std::io::Result<()> {
    // The transmit + virtual-placement APCs carry NO position, so they need the tmux passthrough envelope
    // (tmux would otherwise swallow them) but are sent once per image, not per frame.
    let apc = |bytes: Vec<u8>| if tmux { wrap_tmux(&bytes) } else { bytes };
    for (path, at) in images {
        let id = image_id(path);
        if resident.contains(&id) {
            continue; // already transmitted + virtually placed; the painted cells drive display
        }
        if let Some(png) = read_png(path) {
            out.write_all(&apc(transmit_png(id, &png)))?;
            out.write_all(&apc(virtual_place(id, at.cols, at.rows)))?;
            resident.insert(id);
        }
    }
    out.flush()
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
            s.starts_with("\x1b_Ga=t,f=100,i=7,q=2,m=1;"),
            "first chunk carries the keys"
        );
        assert!(s.contains("\x1b_Gm=0;"), "the last chunk is marked m=0");
        assert!(s.ends_with("\x1b\\"), "each chunk is APC-terminated");
        assert!(!out.is_empty());
    }

    #[test]
    fn place_delete_free_commands() {
        assert_eq!(place(5, 20, 8), b"\x1b_Ga=p,i=5,p=1,q=2,c=20,r=8\x1b\\");
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
    fn graphics_pass_transmits_places_and_degrades_on_load_failure() {
        let at = Placement {
            row: 2,
            col: 0,
            cols: 20,
            rows: 8,
        };
        // A loadable image → transmit + a U=1 VIRTUAL placement (no cursor move / direct place).
        let mut resident = HashSet::new();
        let mut buf = Vec::new();
        graphics_pass(
            &mut buf,
            &[("ok.png".into(), at)],
            &mut resident,
            false,
            |_| Some(vec![1, 2, 3]),
        )
        .unwrap();
        let s = String::from_utf8_lossy(&buf);
        assert!(s.contains("\x1b_Ga=t"), "transmit emitted");
        assert!(s.contains("a=p,U=1"), "virtual placement emitted");
        assert!(resident.len() == 1, "image is now resident");
        // A failed load → no escapes, and the id is left retryable (not resident).
        let mut resident2 = HashSet::new();
        let mut buf2 = Vec::new();
        graphics_pass(
            &mut buf2,
            &[("bad.png".into(), at)],
            &mut resident2,
            false,
            |_| None,
        )
        .unwrap();
        assert!(buf2.is_empty(), "no escapes for an unloadable image");
        assert!(resident2.is_empty(), "failed load stays retryable");
    }

    #[test]
    fn placeholder_cell_and_id_encoding() {
        // A placeholder cell = U+10EEEE + row diacritic + col diacritic (3 chars).
        let c = placeholder_cell(0, 1);
        let chars: Vec<char> = c.chars().collect();
        assert_eq!(chars.len(), 3);
        assert_eq!(chars[0], '\u{10EEEE}');
        assert_eq!(chars[1], DIACRITICS[0]);
        assert_eq!(chars[2], DIACRITICS[1]);
        // The id round-trips through the fg RGB, and virtual_place carries U=1 + the box size.
        assert_eq!(id_rgb(0x0A0B0C), (0x0A, 0x0B, 0x0C));
        assert_eq!(
            virtual_place(9, 20, 8),
            b"\x1b_Ga=p,U=1,i=9,c=20,r=8,q=2\x1b\\"
        );
    }

    #[test]
    fn wrap_tmux_doubles_inner_esc_and_envelopes() {
        // `\x1b_Gx\x1b\\` -> `\x1bPtmux;` + doubled ESCs + `\x1b\\`.
        let w = wrap_tmux(b"\x1b_Gx\x1b\\");
        assert_eq!(w, b"\x1bPtmux;\x1b\x1b_Gx\x1b\x1b\\\x1b\\");
    }

    #[test]
    fn graphics_pass_wraps_for_tmux() {
        let at = Placement {
            row: 0,
            col: 0,
            cols: 10,
            rows: 4,
        };
        let mut resident = HashSet::new();
        let mut buf = Vec::new();
        graphics_pass(
            &mut buf,
            &[("x.png".into(), at)],
            &mut resident,
            true,
            |_| Some(vec![1, 2, 3]),
        )
        .unwrap();
        // The transmit + virtual-placement APCs are wrapped so tmux forwards them to the outer terminal.
        assert!(
            buf.windows(7).any(|w| w == b"\x1bPtmux;"),
            "tmux envelope present"
        );
        // The virtual placement (U=1) is carried through (its bytes have no ESC, so they appear verbatim).
        assert!(
            buf.windows(3).any(|w| w == b"U=1"),
            "virtual placement carried"
        );
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
