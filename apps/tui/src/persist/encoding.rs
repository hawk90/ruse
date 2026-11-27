//! Encoding / line-ending preservation (F-008 acceptance #2).
//!
//! ruse's core is byte-oriented and edits in LF (`\n`); a file opened as CRLF or with a UTF-8 BOM
//! must be written back in *its* style, not silently normalised (VIM-STATE-1: "`dos` fileformat
//! hides `^M`"; TEXT-19: separate encoding/line-endings from document data). [`FileFormat::detect`]
//! reads the original style once at load; [`FileFormat::to_buffer`] strips it for clean in-editor
//! bytes; [`FileFormat::to_disk`] restores it on save. All pure — no IO — so the round-trip is
//! unit-testable.
//!
//! The file's encoding ([`FileFormat::encoding`], RFC-0025) is the outermost step: a read decodes the file
//! (without its BOM) into UTF-8 text before the line endings are looked at, and a write encodes the text
//! after they are restored, then adds the BOM. Until `'fileencoding'` lands (RFC-0025 slice 2) the encoding
//! is always the default, UTF-8 with invalid bytes kept, whose decode and encode are the bytes themselves:
//! a file that is not valid UTF-8 is still preserved byte-for-byte.

use std::borrow::Cow;

use super::codec::{FileEncoding, Unencodable};

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
    /// Vim's `'bomb'`: the file starts with its encoding's BOM ([`FileEncoding::bom`]; `EF BB BF` for
    /// UTF-8), which a write emits again.
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
    /// Vim's `'fileencoding'` (RFC-0025): the encoding the file is read and written in. The default (the
    /// empty name) is UTF-8 with invalid bytes kept. `'binary'` converts nothing, whatever this says.
    pub encoding: FileEncoding,
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
            encoding: FileEncoding::default(),
        }
    }
}

#[cfg(test)]
const BOM: [u8; 3] = [0xEF, 0xBB, 0xBF];

/// `text` line-terminated (INV-TEXT-TERMINATED, RFC-0024): empty, or ending in `\n`. Every path that brings
/// outside text in as a buffer's whole text ends here.
pub fn terminated(mut text: Vec<u8>) -> Vec<u8> {
    if !text.is_empty() && text.last() != Some(&b'\n') {
        text.push(b'\n');
    }
    text
}

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
        FileFormat::detect_in(raw, ffs, default, FileEncoding::default())
    }

    /// [`FileFormat::detect_with`] of a file in `encoding`: its BOM, then the line endings of its text, which
    /// are found after decoding (in UTF-16 a newline is not the byte `0A`).
    pub fn detect_in(raw: &[u8], ffs: &str, default: Eol, encoding: FileEncoding) -> FileFormat {
        let bom = encoding.bom().is_some_and(|b| raw.starts_with(b));
        let text = FileFormat {
            bom,
            encoding,
            ..FileFormat::default()
        }
        .text(raw);
        let body = &text[..];
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
        FileFormat {
            eol,
            bom,
            final_newline: true,
            eof,
            binary: false,
            encoding,
        }
        .reading(raw)
    }

    /// The text of `raw` in this format's encoding: without the BOM (when `'bomb'` and the file has it), and
    /// decoded, an undecodable byte kept as it is. The file's bytes as they are for UTF-8 and `'binary'`.
    fn text(self, raw: &[u8]) -> Cow<'_, [u8]> {
        if self.binary {
            return Cow::Borrowed(raw);
        }
        let body = match self.encoding.bom() {
            Some(b) if self.bom && raw.starts_with(b) => &raw[b.len()..],
            _ => raw,
        };
        self.encoding.text(body)
    }

    /// `self` with `'endofline'` as `raw` read in this format ends: after the BOM and `'endoffile'`'s
    /// CTRL-Z, the file's last byte is its line break (`\r` for mac, else `\n`), or the file is empty. A
    /// read whose format is forced (`++ff=`, `++bin`, `find-file-literally`) calls it again, because the
    /// buffer is line-terminated (RFC-0024) and a wrong flag would drop or add a byte on the next write.
    #[must_use]
    pub fn reading(self, raw: &[u8]) -> FileFormat {
        let text = self.text(raw);
        let body = match text.split_last() {
            Some((&CTRL_Z, rest)) if !self.binary && self.eof => rest,
            _ => &text[..],
        };
        let last = if !self.binary && self.eol == Eol::Cr {
            b'\r'
        } else {
            b'\n'
        };
        FileFormat {
            final_newline: body.is_empty() || body.last() == Some(&last),
            ..self
        }
    }

    /// The shape of a file read with `'binary'` (`++bin`, `ruse -b`): no conversion at all, and
    /// `'endofline'` as the file ends.
    pub fn binary(raw: &[u8]) -> FileFormat {
        FileFormat {
            eol: Eol::Lf,
            bom: false,
            final_newline: true,
            eof: false,
            binary: true,
            encoding: FileEncoding::default(),
        }
        .reading(raw)
    }

    /// Normalise raw file bytes into clean editor bytes: drop the BOM, fold `\r\n` → `\n`. The
    /// editor never sees `^M` or a BOM char; the style is remembered in `self` for save. The result is
    /// line-terminated (INV-TEXT-TERMINATED, RFC-0024): a last line without a newline gets one, and
    /// `'endofline'` (`final_newline`, from [`FileFormat::detect`]) remembers that the file had none.
    pub fn to_buffer(self, raw: &[u8]) -> Vec<u8> {
        terminated(self.decode(raw))
    }

    /// [`FileFormat::to_buffer`] before the final newline is added: the buffer text a build before RFC-0024
    /// started from, which a trace recorded then hashes (`ruse --replay`).
    pub fn decode(self, raw: &[u8]) -> Vec<u8> {
        if self.binary {
            return raw.to_vec();
        }
        let text = self.text(raw);
        let body = match text.split_last() {
            Some((&CTRL_Z, rest)) if self.eof => rest,
            _ => &text[..],
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

    /// Restore the original style for writing to disk: drop the buffer's final newline without
    /// `'endofline'`, re-expand `\n` → `\r\n` for a CRLF file, encode the text in the file's encoding and
    /// re-prepend its BOM. The inverse of [`to_buffer`]. A character the encoding cannot hold fails the whole
    /// write ([`Unencodable`], with its line in the file; `E513`, RFC-0025 §4.1); the default encoding holds
    /// every text, so it never fails.
    pub fn to_disk(self, buffer: &[u8]) -> Result<Vec<u8>, Unencodable> {
        let buffer = match buffer.strip_suffix(b"\n") {
            Some(body) if !self.final_newline => body,
            _ => buffer,
        };
        if self.binary {
            return Ok(buffer.to_vec());
        }
        let bom = match self.encoding.bom() {
            Some(b) if self.bom => b,
            _ => &[],
        };
        let mut out = Vec::with_capacity(buffer.len() + bom.len());
        out.extend_from_slice(bom);
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
        if self.encoding.is_utf8() {
            return Ok(out);
        }
        let lines = &out[bom.len()..];
        let encoded = self.encoding.encode(lines).map_err(|e| match self.eol {
            // A mac file's lines end in `\r` by now.
            Eol::Cr => Unencodable {
                line: 1 + lines[..e.offset].iter().filter(|&&b| b == b'\r').count(),
                ..e
            },
            Eol::Lf | Eol::Crlf => e,
        })?;
        Ok([bom, &encoded].concat())
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
                proptest::sample::select(b"a\r\n\r\n\xEF\xBB\xBF\x00\xFF\x1A".to_vec()), 0..24),
            bom in proptest::prelude::any::<bool>(),
            ffs in proptest::sample::select(vec!["unix,dos", "unix,dos,mac", "mac", "dos", "unix"]),
        ) {
            let raw = if bom { [BOM.as_slice(), &raw].concat() } else { raw };
            // Every read: detected under any 'fileformats', and `++bin`.
            for fmt in [FileFormat::detect_with(&raw, ffs, Eol::Lf), FileFormat::binary(&raw)] {
                let buffer = fmt.to_buffer(&raw);
                // INV-TEXT-TERMINATED (RFC-0024): the buffer is empty or ends in a newline.
                proptest::prop_assert!(buffer.is_empty() || buffer.ends_with(b"\n"), "{:?}", buffer);
                // Byte for byte when the format was detected (several 'fileformats') or `++bin`; one forced
                // format converts on write, as Vim's does. A mac read of a file with a bare `\n` cannot
                // round-trip either, before RFC-0024 too: the buffer's `\n` is the line break, which a mac
                // write turns into `\r` (Vim keeps such a byte as NUL).
                let detected = fmt.binary || ffs.contains(',');
                if detected && !(fmt.eol == Eol::Cr && !fmt.binary && raw.contains(&b'\n')) {
                    proptest::prop_assert_eq!(fmt.to_disk(&buffer).unwrap(), raw.clone(), "{:?}", fmt);
                }
            }
        }
    }

    #[test]
    fn detects_and_round_trips_crlf() {
        let raw = b"one\r\ntwo\r\nthree";
        let fmt = FileFormat::detect(raw);
        assert_eq!(fmt.eol, Eol::Crlf);
        assert!(!fmt.bom);
        let buf = fmt.to_buffer(raw);
        // Clean LF in the editor, line-terminated (RFC-0024): the missing final newline is `'endofline'`.
        assert!(!fmt.final_newline);
        assert_eq!(buf, b"one\ntwo\nthree\n");
        assert_eq!(fmt.to_disk(&buf).unwrap(), raw); // exact original on save
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
        assert_eq!(fmt.to_disk(&buf).unwrap(), raw);
    }

    #[test]
    fn edited_crlf_file_saves_all_lines_crlf() {
        // Open CRLF, edit adds an LF line; save must normalise the NEW line to CRLF too.
        let fmt = FileFormat::detect(b"a\r\nb\r\n");
        let edited_buffer = b"a\nb\nc\n"; // 'c' typed with the editor's LF
        assert_eq!(fmt.to_disk(edited_buffer).unwrap(), b"a\r\nb\r\nc\r\n");
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
            assert_eq!(fmt.to_disk(&fmt.to_buffer(raw)).unwrap(), raw);
        }
    }

    #[test]
    fn empty_file_is_lf_no_bom() {
        let fmt = FileFormat::detect(b"");
        assert_eq!(fmt, FileFormat::default());
        assert_eq!(fmt.to_disk(b"").unwrap(), b"");
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
        assert_eq!(mac.to_disk(&mac.to_buffer(m)).unwrap(), m);
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

    /// `'binary'`: the buffer is the file's bytes, BOM and CR included, and a write gives them back. Only
    /// the missing final newline becomes `'endofline'`, as in any buffer (RFC-0024).
    #[test]
    fn binary_converts_nothing() {
        let raw = b"\xEF\xBB\xBFa\r\nb";
        let f = FileFormat::binary(raw);
        assert!(!f.final_newline);
        assert_eq!(f.to_buffer(raw), b"\xEF\xBB\xBFa\r\nb\n");
        assert_eq!(f.to_disk(&f.to_buffer(raw)).unwrap(), raw);
    }

    /// nvim v0.12.6: a DOS file ending in CTRL-Z has `'endoffile'`; the byte is not text and comes back on
    /// a write. A Unix file's CTRL-Z is text.
    #[test]
    fn endoffile_is_a_dos_files_last_ctrl_z() {
        let raw = b"one\r\ntwo\r\n\x1a";
        let fmt = FileFormat::detect(raw);
        assert!(fmt.eof && fmt.final_newline && fmt.eol == Eol::Crlf);
        assert_eq!(fmt.to_buffer(raw), b"one\ntwo\n");
        assert_eq!(fmt.to_disk(&fmt.to_buffer(raw)).unwrap(), raw);
        let unix = FileFormat::detect(b"one\ntwo\n\x1a");
        assert!(!unix.eof && !unix.final_newline);
        assert_eq!(unix.to_buffer(b"one\ntwo\n\x1a"), b"one\ntwo\n\x1a\n");
        assert_eq!(
            unix.to_disk(&unix.to_buffer(b"one\ntwo\n\x1a")).unwrap(),
            b"one\ntwo\n\x1a"
        );
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
            crlf.to_disk(&FileFormat::normalize(b"X\r\nY\r\n")).unwrap(),
            b"X\r\nY\r\n"
        );
    }

    fn enc(name: &str) -> FileEncoding {
        FileEncoding::from_name(name).unwrap()
    }

    /// RFC-0025 slice 1: every format has the default encoding, which reads and writes the file's bytes.
    #[test]
    fn every_format_has_the_default_encoding() {
        for raw in [
            &b""[..],
            b"a\r\n",
            b"\xEF\xBB\xBFa\n",
            b"caf\xE9\n",
            b"\xFF\xFE\x00a",
        ] {
            assert_eq!(FileFormat::detect(raw).encoding, FileEncoding::default());
            assert_eq!(FileFormat::binary(raw).encoding, FileEncoding::default());
        }
        assert_eq!(FileFormat::default().encoding.name(), "");
    }

    /// The decode comes before the line endings are looked at, and the encode after they are restored:
    /// a UTF-16LE CRLF file with its BOM reads as LF text and writes back its bytes.
    #[test]
    fn line_endings_are_found_in_the_decoded_text() {
        let u16le = enc("utf-16le");
        let text = "café\r\n한\r\n\u{1A}";
        let raw = [&b"\xFF\xFE"[..], &u16le.encode(text.as_bytes()).unwrap()].concat();
        let fmt = FileFormat::detect_in(&raw, "unix,dos", Eol::Lf, u16le);
        assert!(fmt.bom && fmt.eof && fmt.final_newline && fmt.eol == Eol::Crlf);
        let buffer = fmt.to_buffer(&raw);
        assert_eq!(buffer, "café\n한\n".as_bytes());
        assert_eq!(fmt.to_disk(&buffer).unwrap(), raw);
        // Without `'endofline'` the last line is written without its newline, in the encoding.
        let latin1 =
            FileFormat::detect_in(b"caf\xE9\nna\xEFve", "unix,dos", Eol::Lf, enc("latin1"));
        assert!(!latin1.final_newline && !latin1.bom);
        let buffer = latin1.to_buffer(b"caf\xE9\nna\xEFve");
        assert_eq!(buffer, "café\nnaïve\n".as_bytes());
        assert_eq!(latin1.to_disk(&buffer).unwrap(), b"caf\xE9\nna\xEFve");
        // `'bomb'` in an encoding without a BOM writes none.
        let f = FileFormat {
            bom: true,
            ..latin1
        };
        assert_eq!(f.to_disk(b"a\n").unwrap(), b"a");
        // `'binary'` converts nothing, whatever the encoding.
        let bin = FileFormat {
            binary: true,
            ..FileFormat::binary(&raw)
        };
        let bin = FileFormat {
            encoding: u16le,
            ..bin
        };
        assert_eq!(bin.to_disk(&bin.to_buffer(&raw)).unwrap(), raw);
    }

    /// A character the encoding cannot hold fails the write with its line, in every line ending.
    #[test]
    fn an_unencodable_character_fails_the_write_with_its_line() {
        for eol in [Eol::Lf, Eol::Crlf, Eol::Cr] {
            let f = FileFormat {
                eol,
                encoding: enc("latin1"),
                ..FileFormat::default()
            };
            let e = f.to_disk("a\nb\nc 한\n".as_bytes()).unwrap_err();
            assert_eq!((e.ch, e.line), ('한', 3), "{eol:?}");
        }
    }

    proptest::proptest! {
        #![proptest_config(crate::test_cases(256))]

        /// F-008 through the encodings: a file written from any text in an encoding, with any line endings
        /// and BOM, is read back in that encoding to a buffer that writes the same bytes.
        #[test]
        fn an_unmodified_buffer_in_an_encoding_writes_back_its_bytes(
            text in proptest::collection::vec(
                proptest::sample::select(vec!["a", "\n", "\r", "é", "한", "\u{1A}", "\0"]), 0..16),
            name in proptest::sample::select(vec!["utf-16", "utf-16le", "ucs-4", "ucs-4le", "latin1", "cp1252", "euc-kr"]),
            eol in proptest::sample::select(vec![Eol::Lf, Eol::Crlf]),
            bom in proptest::prelude::any::<bool>(),
        ) {
            let e = enc(name);
            let text: String = text.concat();
            let mut lines = text.replace("\r\n", "\n");
            if eol == Eol::Crlf {
                lines = lines.replace('\r', "").replace('\n', "\r\n");
            }
            let Ok(body) = e.encode(lines.as_bytes()) else {
                return Ok(()); // a character the encoding cannot hold: no such file
            };
            let bom = bom && e.bom().is_some();
            let raw = if bom { [e.bom().unwrap(), &body].concat() } else { body.to_vec() };
            let fmt = FileFormat::detect_in(&raw, "unix,dos", Eol::Lf, e);
            proptest::prop_assert_eq!(fmt.bom, bom);
            let buffer = fmt.to_buffer(&raw);
            proptest::prop_assert!(buffer.is_empty() || buffer.ends_with(b"\n"));
            proptest::prop_assert_eq!(fmt.to_disk(&buffer).unwrap(), raw, "{:?}", fmt);
        }
    }
}
