//! Encoding / line-ending preservation (F-008 acceptance #2).
//!
//! ruse's core is byte-oriented and edits in LF (`\n`); a file opened as CRLF or with a UTF-8 BOM
//! must be written back in *its* style, not silently normalised (VIM-STATE-1: "`dos` fileformat
//! hides `^M`"; TEXT-19: separate encoding/line-endings from document data). [`FileFormat::detect`]
//! reads the original style once at load; [`FileFormat::to_buffer`] strips it for clean in-editor
//! bytes; [`FileFormat::to_disk`] restores it on save. All pure — no IO — so the round-trip is
//! unit-testable.
//!
//! Scope (MVP-minimal, per the post-MVP design pass): UTF-8 + optional BOM, and LF vs CRLF. Other
//! encodings (UTF-16, legacy codepages) are a post-MVP widening; a file that is not valid UTF-8 is
//! still preserved byte-for-byte because `to_buffer`/`to_disk` only touch the BOM and `\n`/`\r\n`.

/// The line-ending style of a file.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Eol {
    /// Unix `\n`.
    #[default]
    Lf,
    /// DOS/Windows `\r\n`.
    Crlf,
}

/// The original on-disk shape to restore on save.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct FileFormat {
    pub eol: Eol,
    /// A leading UTF-8 BOM (`EF BB BF`) was present and must be re-emitted.
    pub bom: bool,
    /// Vim's `'endofline'` (#1107): a write ends the last line with a newline. Set when the file was
    /// missing, empty, or ended in one; clear only for a file opened without one, which ruse keeps that way
    /// (byte-preserve, as nvim with `'nofixeol'`). So a new or emptied-and-retyped buffer saves as `a\n`.
    pub final_newline: bool,
}

/// A new buffer: LF, no BOM, and `'endofline'` set, as in Vim.
impl Default for FileFormat {
    fn default() -> Self {
        FileFormat {
            eol: Eol::Lf,
            bom: false,
            final_newline: true,
        }
    }
}

const BOM: [u8; 3] = [0xEF, 0xBB, 0xBF];

impl FileFormat {
    /// Read the style of the original file. Like Vim's `fileformats=unix,dos`, a file is CRLF only when
    /// every line break is `\r\n`; one bare `\n` makes it LF, with each `\r` kept as buffer text, so a save
    /// without edits writes the same bytes (#849). A new/empty file is `Lf`.
    pub fn detect(raw: &[u8]) -> FileFormat {
        let bom = raw.starts_with(&BOM);
        let body = if bom { &raw[BOM.len()..] } else { raw };
        let crlf = body.windows(2).filter(|w| w == b"\r\n").count();
        // Bare `\n` = total `\n` minus those that are part of a `\r\n`.
        let lf_total = ruse_core::pos::count_newlines(body);
        let bare_lf = lf_total.saturating_sub(crlf);
        let eol = if crlf > 0 && bare_lf == 0 {
            Eol::Crlf
        } else {
            Eol::Lf
        };
        let final_newline = body.is_empty() || body.ends_with(b"\n");
        FileFormat {
            eol,
            bom,
            final_newline,
        }
    }

    /// Normalise raw file bytes into clean editor bytes: drop the BOM, fold `\r\n` → `\n`. The
    /// editor never sees `^M` or a BOM char; the style is remembered in `self` for save.
    pub fn to_buffer(self, raw: &[u8]) -> Vec<u8> {
        let body = if self.bom && raw.starts_with(&BOM) {
            &raw[BOM.len()..]
        } else {
            raw
        };
        if self.eol == Eol::Crlf {
            // Strip only the `\r` that precedes a `\n`; leave lone `\r` (old-Mac data) untouched.
            let mut out = Vec::with_capacity(body.len());
            let mut i = 0;
            while i < body.len() {
                if body[i] == b'\r' && body.get(i + 1) == Some(&b'\n') {
                    i += 1; // skip the \r; the \n is copied next iteration
                    continue;
                }
                out.push(body[i]);
                i += 1;
            }
            out
        } else {
            body.to_vec()
        }
    }

    /// Clean editor bytes for text read INTO an existing buffer (`:r {file}`): detect the read file's own
    /// style and strip it, as opening the file would, so the target buffer's format applies on save. Like
    /// Vim's `:r` with `fileformats=unix,dos`, a CRLF file loses its `\r` and a leading BOM is dropped (#860).
    pub fn normalize(raw: &[u8]) -> Vec<u8> {
        FileFormat::detect(raw).to_buffer(raw)
    }

    /// Restore the original style for writing to disk: re-expand `\n` → `\r\n` for a CRLF file and
    /// re-prepend the BOM. The inverse of [`to_buffer`].
    pub fn to_disk(self, buffer: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(buffer.len() + if self.bom { 3 } else { 0 });
        if self.bom {
            out.extend_from_slice(&BOM);
        }
        if self.eol == Eol::Crlf {
            for &b in buffer {
                if b == b'\n' {
                    out.push(b'\r');
                }
                out.push(b);
            }
        } else {
            out.extend_from_slice(buffer);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    proptest::proptest! {
        #![proptest_config(crate::test_cases(512))]

        /// An unmodified buffer writes back the exact bytes it was read from (#909): for any file, the
        /// detected format strips to buffer bytes that restore to the original, and a file read with a
        /// final newline gives a buffer that already ends in one (so the save step adds none). Over an
        /// alphabet of the bytes the format logic looks at: CR, LF, the BOM's bytes, NUL and a stray byte.
        #[test]
        fn an_unmodified_buffer_writes_back_its_bytes(
            raw in proptest::collection::vec(
                proptest::sample::select(b"a\r\n\r\n\xEF\xBB\xBF\x00\xFF".to_vec()), 0..24),
            bom in proptest::prelude::any::<bool>(),
        ) {
            let raw = if bom { [BOM.as_slice(), &raw].concat() } else { raw };
            let fmt = FileFormat::detect(&raw);
            let buffer = fmt.to_buffer(&raw);
            proptest::prop_assert!(!fmt.final_newline || buffer.is_empty() || buffer.ends_with(b"\n"));
            proptest::prop_assert_eq!(fmt.to_disk(&buffer), raw);
        }
    }

    #[test]
    fn detects_and_round_trips_crlf() {
        let raw = b"one\r\ntwo\r\nthree";
        let fmt = FileFormat::detect(raw);
        assert_eq!(fmt.eol, Eol::Crlf);
        assert!(!fmt.bom);
        let buf = fmt.to_buffer(raw);
        assert_eq!(buf, b"one\ntwo\nthree"); // clean LF in the editor
        assert_eq!(fmt.to_disk(&buf), raw); // exact original on save
    }

    #[test]
    fn detects_and_round_trips_bom_lf() {
        let mut raw = BOM.to_vec();
        raw.extend_from_slice(b"hello\nworld\n");
        let fmt = FileFormat::detect(&raw);
        assert_eq!(fmt.eol, Eol::Lf);
        assert!(fmt.bom);
        let buf = fmt.to_buffer(&raw);
        assert_eq!(buf, b"hello\nworld\n");
        assert_eq!(fmt.to_disk(&buf), raw);
    }

    #[test]
    fn edited_crlf_file_saves_all_lines_crlf() {
        // Open CRLF, edit adds an LF line; save must normalise the NEW line to CRLF too.
        let fmt = FileFormat::detect(b"a\r\nb\r\n");
        let edited_buffer = b"a\nb\nc\n"; // 'c' typed with the editor's LF
        assert_eq!(fmt.to_disk(edited_buffer), b"a\r\nb\r\nc\r\n");
    }

    #[test]
    fn lone_cr_data_is_preserved_not_treated_as_eol() {
        let raw = b"key\rvalue\nnext\n"; // a bare \r inside LF content
        let fmt = FileFormat::detect(raw);
        assert_eq!(fmt.eol, Eol::Lf);
        assert_eq!(fmt.to_buffer(raw), raw); // untouched
    }

    /// #849: a mixed file is LF, so a save without edits leaves every byte as it was.
    #[test]
    fn mixed_line_endings_round_trip_unchanged() {
        for raw in [&b"a\r\nb\r\nc\n"[..], b"a\nb\r\n", b"a\r\nb\nc\r\n"] {
            let fmt = FileFormat::detect(raw);
            assert_eq!(fmt.eol, Eol::Lf, "{raw:?}");
            assert_eq!(fmt.to_disk(&fmt.to_buffer(raw)), raw);
        }
    }

    #[test]
    fn empty_file_is_lf_no_bom() {
        let fmt = FileFormat::detect(b"");
        assert_eq!(fmt, FileFormat::default());
        assert_eq!(fmt.to_disk(b""), b"");
    }

    /// #1107: `'endofline'` follows the file as read; a missing, empty or BOM-only file has it.
    #[test]
    fn final_newline_follows_the_file() {
        for (raw, want) in [
            (&b""[..], true),
            (b"\xEF\xBB\xBF", true),
            (b"a\n", true),
            (b"a\r\n", true),
            (b"a", false),
            (b"a\nb", false),
            (b"a\r", false),
        ] {
            assert_eq!(FileFormat::detect(raw).final_newline, want, "{raw:?}");
        }
        assert!(FileFormat::default().final_newline);
    }

    /// #860: `:r` text is cleaned by its own format, so the target buffer's save adds exactly one `\r`.
    #[test]
    fn normalize_strips_the_read_files_own_format() {
        assert_eq!(FileFormat::normalize(b"X\r\nY\r\n"), b"X\nY\n");
        assert_eq!(FileFormat::normalize(b"\xEF\xBB\xBFX\r\nY\r\n"), b"X\nY\n");
        assert_eq!(FileFormat::normalize(b"\xEF\xBB\xBFX\nY\n"), b"X\nY\n");
        // A mixed file is LF: its `\r` is text, as on open.
        assert_eq!(FileFormat::normalize(b"X\r\nY\n"), b"X\r\nY\n");
        let crlf = FileFormat::detect(b"one\r\n");
        assert_eq!(
            crlf.to_disk(&FileFormat::normalize(b"X\r\nY\r\n")),
            b"X\r\nY\r\n"
        );
    }
}
