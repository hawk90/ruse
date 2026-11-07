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
    /// Classic Mac `\r` (#619, Vim's `fileformat=mac`).
    Cr,
}

impl Eol {
    /// Vim's `'fileformat'` name.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Eol::Lf => "unix",
            Eol::Crlf => "dos",
            Eol::Cr => "mac",
        }
    }

    /// The `'fileformat'` called `name`.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Eol> {
        Some(match name {
            "unix" => Eol::Lf,
            "dos" => Eol::Crlf,
            "mac" => Eol::Cr,
            _ => return None,
        })
    }
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
    /// Vim's `'endoffile'` (#619): a DOS file ended with a CTRL-Z, which is not buffer text and is written
    /// back after the last line (unless `'fixeol'`, as nvim).
    pub eof: bool,
    /// Vim's `'binary'` (#619): the bytes are the buffer as they are, with no line-ending, BOM or final
    /// newline conversion either way.
    pub binary: bool,
}

/// The DOS end-of-file byte `'endoffile'` stands for.
pub const CTRL_Z: u8 = 0x1A;

/// A new buffer: LF, no BOM, and `'endofline'` set, as in Vim.
impl Default for FileFormat {
    fn default() -> Self {
        FileFormat {
            eol: Eol::Lf,
            bom: false,
            final_newline: true,
            eof: false,
            binary: false,
        }
    }
}

const BOM: [u8; 3] = [0xEF, 0xBB, 0xBF];

impl FileFormat {
    /// Read the style of the original file. Like Vim's `fileformats=unix,dos`, a file is CRLF only when
    /// every line break is `\r\n`; one bare `\n` makes it LF, with each `\r` kept as buffer text, so a save
    /// without edits writes the same bytes (#849). A new/empty file is `Lf`.
    pub fn detect(raw: &[u8]) -> FileFormat {
        FileFormat::detect_with(raw, "unix,dos", Eol::Lf)
    }

    /// [`FileFormat::detect`] under Vim's `'fileformats'` `ffs` (#619): an empty list reads every file as
    /// `default` (`'fileformat'`), one name reads it as that format. With several: `dos` when listed and
    /// every line break is `\r\n`; else `unix` when listed and the file has a `\n`; `mac` when listed and the
    /// file has `\r` but no `\n`; else the first listed (a file with no line break at all), as nvim v0.12.6.
    pub fn detect_with(raw: &[u8], ffs: &str, default: Eol) -> FileFormat {
        let bom = raw.starts_with(&BOM);
        let body = if bom { &raw[BOM.len()..] } else { raw };
        let crlf = body.windows(2).filter(|w| w == b"\r\n").count();
        // Bare `\n` = total `\n` minus those that are part of a `\r\n`.
        let lf_total = ruse_core::pos::count_newlines(body);
        let bare_lf = lf_total.saturating_sub(crlf);
        let listed: Vec<Eol> = ffs
            .split(',')
            .filter_map(|n| Eol::from_name(n.trim()))
            .collect();
        let eol = match listed.as_slice() {
            [] => default,
            [one] => *one,
            many => {
                let has = |e: Eol| many.contains(&e);
                if has(Eol::Crlf) && crlf > 0 && bare_lf == 0 {
                    Eol::Crlf
                } else if lf_total > 0 && has(Eol::Lf) {
                    Eol::Lf
                } else if lf_total == 0 && has(Eol::Cr) && body.contains(&b'\r') {
                    Eol::Cr
                } else if lf_total > 0 && has(Eol::Crlf) {
                    Eol::Crlf
                } else {
                    many[0]
                }
            }
        };
        // nvim: a DOS file's last byte CTRL-Z is `'endoffile'`, not text.
        let eof = eol == Eol::Crlf && body.last() == Some(&CTRL_Z);
        let body = if eof { &body[..body.len() - 1] } else { body };
        let last = if eol == Eol::Cr { b'\r' } else { b'\n' };
        let final_newline = body.is_empty() || body.last() == Some(&last);
        FileFormat {
            eol,
            bom,
            final_newline,
            eof,
            binary: false,
        }
    }

    /// The shape of a file read with `'binary'` (`++bin`, `ruse -b`): no conversion at all, and
    /// `'endofline'` as the file ends.
    pub fn binary(raw: &[u8]) -> FileFormat {
        FileFormat {
            eol: Eol::Lf,
            bom: false,
            final_newline: raw.is_empty() || raw.ends_with(b"\n"),
            eof: false,
            binary: true,
        }
    }

    /// Normalise raw file bytes into clean editor bytes: drop the BOM, fold `\r\n` → `\n`. The
    /// editor never sees `^M` or a BOM char; the style is remembered in `self` for save.
    pub fn to_buffer(self, raw: &[u8]) -> Vec<u8> {
        if self.binary {
            return raw.to_vec();
        }
        let body = if self.bom && raw.starts_with(&BOM) {
            &raw[BOM.len()..]
        } else {
            raw
        };
        let body = match body.split_last() {
            Some((&CTRL_Z, rest)) if self.eof => rest,
            _ => body,
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
        } else if self.eol == Eol::Cr {
            body.iter()
                .map(|&b| if b == b'\r' { b'\n' } else { b })
                .collect()
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
        if self.binary {
            return buffer.to_vec();
        }
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
        } else if self.eol == Eol::Cr {
            out.extend(buffer.iter().map(|&b| if b == b'\n' { b'\r' } else { b }));
        } else {
            out.extend_from_slice(buffer);
        }
        if self.eof {
            out.push(CTRL_Z);
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

    /// nvim v0.12.6 `'fileformats'`: the default `unix,dos` reads a CR-only file as unix (one line); with
    /// `mac` listed it is mac; one name forces it; an empty list takes `'fileformat'`; a file with no line
    /// break takes the first name.
    #[test]
    fn fileformats_decide_the_format_as_nvim() {
        let m = b"a\rb\r";
        assert_eq!(FileFormat::detect(m).eol, Eol::Lf);
        let mac = FileFormat::detect_with(m, "unix,dos,mac", Eol::Lf);
        assert_eq!(mac.eol, Eol::Cr);
        assert!(mac.final_newline);
        assert_eq!(mac.to_buffer(m), b"a\nb\n");
        assert_eq!(mac.to_disk(&mac.to_buffer(m)), m);
        assert_eq!(
            FileFormat::detect_with(b"a\nb\n", "dos", Eol::Lf).eol,
            Eol::Crlf
        );
        assert_eq!(
            FileFormat::detect_with(b"a\r\n", "", Eol::Crlf).eol,
            Eol::Crlf
        );
        assert_eq!(
            FileFormat::detect_with(b"abc", "dos,unix", Eol::Lf).eol,
            Eol::Crlf
        );
        assert_eq!(
            FileFormat::detect_with(b"a\r\nb\n", "unix,dos", Eol::Lf).eol,
            Eol::Lf
        );
    }

    /// `'binary'`: the buffer is the file's bytes, BOM and CR included, and a write gives them back.
    #[test]
    fn binary_converts_nothing() {
        let raw = b"\xEF\xBB\xBFa\r\nb";
        let f = FileFormat::binary(raw);
        assert!(!f.final_newline);
        assert_eq!(f.to_buffer(raw), raw);
        assert_eq!(f.to_disk(raw), raw);
    }

    /// nvim v0.12.6: a DOS file ending in CTRL-Z has `'endoffile'`; the byte is not text and comes back on
    /// a write. A Unix file's CTRL-Z is text.
    #[test]
    fn endoffile_is_a_dos_files_last_ctrl_z() {
        let raw = b"one\r\ntwo\r\n\x1a";
        let fmt = FileFormat::detect(raw);
        assert!(fmt.eof && fmt.final_newline && fmt.eol == Eol::Crlf);
        assert_eq!(fmt.to_buffer(raw), b"one\ntwo\n");
        assert_eq!(fmt.to_disk(&fmt.to_buffer(raw)), raw);
        let unix = FileFormat::detect(b"one\ntwo\n\x1a");
        assert!(!unix.eof && !unix.final_newline);
        assert_eq!(unix.to_buffer(b"one\ntwo\n\x1a"), b"one\ntwo\n\x1a");
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
