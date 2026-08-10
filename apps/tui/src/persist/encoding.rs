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
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct FileFormat {
    pub eol: Eol,
    /// A leading UTF-8 BOM (`EF BB BF`) was present and must be re-emitted.
    pub bom: bool,
}

const BOM: [u8; 3] = [0xEF, 0xBB, 0xBF];

impl FileFormat {
    /// Read the style of the original file. EOL is decided by majority so a stray lone `\n` in an
    /// otherwise-CRLF file does not flip the verdict (and vice-versa). A new/empty file is `Lf`.
    pub fn detect(raw: &[u8]) -> FileFormat {
        let bom = raw.starts_with(&BOM);
        let body = if bom { &raw[BOM.len()..] } else { raw };
        let crlf = body.windows(2).filter(|w| w == b"\r\n").count();
        // Bare `\n` = total `\n` minus those that are part of a `\r\n`.
        let lf_total = body.iter().filter(|&&b| b == b'\n').count();
        let bare_lf = lf_total.saturating_sub(crlf);
        let eol = if crlf > bare_lf { Eol::Crlf } else { Eol::Lf };
        FileFormat { eol, bom }
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

    #[test]
    fn empty_file_is_lf_no_bom() {
        let fmt = FileFormat::detect(b"");
        assert_eq!(
            fmt,
            FileFormat {
                eol: Eol::Lf,
                bom: false
            }
        );
        assert_eq!(fmt.to_disk(b""), b"");
    }
}
