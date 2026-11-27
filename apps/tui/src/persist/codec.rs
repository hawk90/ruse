//! File encodings at the read/write boundary (RFC-0025 §2, D-066): ruse's one name table and its codecs.
//!
//! A file's encoding is named by its canonical name ([`FileEncoding::name`]), a row of [`ROWS`]; the rest of
//! ruse never sees a codec. This is the only module that names an `encoding_rs` type (D-034 Adapter,
//! `DEP-ENCODING`): a user's name is looked up in ruse's table, never passed to `Encoding::for_label`, which
//! maps `latin1` to windows-1252 and `utf-16` to little-endian. ISO-8859-1, UTF-16 and UTF-32 are written
//! here; the WHATWG legacy sets come from `encoding_rs`.
//!
//! Text in ruse is UTF-8 bytes in which a byte outside a valid sequence is allowed (shown `<xx>`). A decode
//! keeps a byte the encoding cannot decode as it is; an encode writes such a byte as it is, in every
//! encoding, so the round trip of a kept byte holds (RFC-0025 §3.1, §4).
//!
//! The empty name is the encoding of every file before RFC-0025: UTF-8 with invalid bytes kept. It is what a
//! journal or a format with no encoding means (RFC-0025 §7).

use std::borrow::Cow;

use encoding_rs as e;
use encoding_rs::{DecoderResult, EncoderResult, Encoding};

/// How a row's text becomes bytes and back.
#[derive(Clone, Copy)]
enum Codec {
    /// UTF-8: the text is the bytes.
    Utf8,
    /// ISO-8859-1: every byte is the code point of its value, so every file decodes.
    Latin1,
    /// UTF-16 (`pairs`) or UCS-2 (no surrogate pairs: a character above U+FFFF cannot be written, and a
    /// surrogate in the file is not decoded).
    Utf16 { be: bool, pairs: bool },
    /// UTF-32 / UCS-4.
    Utf32 { be: bool },
    /// An `encoding_rs` codec. `exact`: every byte sequence it decodes encodes back to the same bytes. The
    /// multi-byte sets are not exact (Shift_JIS decodes the NEC and IBM duplicates, Big5 the HKSCS pairs it
    /// cannot encode), so a file read in one is accepted only when it encodes back to its own bytes.
    Whatwg { enc: &'static Encoding, exact: bool },
}

/// One row of the name table: a canonical name (what `:set fenc?` shows) and its codec.
struct Row {
    name: &'static str,
    codec: Codec,
}

const fn row(name: &'static str, codec: Codec) -> Row {
    Row { name, codec }
}

const fn web(name: &'static str, enc: &'static Encoding) -> Row {
    row(name, Codec::Whatwg { enc, exact: true })
}

/// A multi-byte WHATWG set whose decoder accepts sequences its encoder does not give back (probed
/// exhaustively over all byte pairs with `encoding_rs` 0.8.44).
const fn web_inexact(name: &'static str, enc: &'static Encoding) -> Row {
    row(name, Codec::Whatwg { enc, exact: false })
}

/// The name table. Canonical names are nvim v0.12.6's: a name nvim folds is folded to the same name
/// ([`ALIASES`]), and a name nvim keeps as given (an iconv name such as `windows-1252` or `cp932`) is its
/// own row. Row 0 is the empty name.
static ROWS: [Row; 60] = [
    row("", Codec::Utf8),
    row("utf-8", Codec::Utf8),
    row("latin1", Codec::Latin1),
    row("l1", Codec::Latin1),
    row(
        "utf-16",
        Codec::Utf16 {
            be: true,
            pairs: true,
        },
    ),
    row(
        "utf-16le",
        Codec::Utf16 {
            be: false,
            pairs: true,
        },
    ),
    row(
        "ucs-2",
        Codec::Utf16 {
            be: true,
            pairs: false,
        },
    ),
    row(
        "ucs-2le",
        Codec::Utf16 {
            be: false,
            pairs: false,
        },
    ),
    row("ucs-4", Codec::Utf32 { be: true }),
    row("ucs-4le", Codec::Utf32 { be: false }),
    web("cp1250", &e::WINDOWS_1250_INIT),
    web("cp1251", &e::WINDOWS_1251_INIT),
    web("cp1252", &e::WINDOWS_1252_INIT),
    web("cp1253", &e::WINDOWS_1253_INIT),
    web("cp1254", &e::WINDOWS_1254_INIT),
    web("cp1255", &e::WINDOWS_1255_INIT),
    web("cp1256", &e::WINDOWS_1256_INIT),
    web("cp1257", &e::WINDOWS_1257_INIT),
    web("cp1258", &e::WINDOWS_1258_INIT),
    web("windows-1250", &e::WINDOWS_1250_INIT),
    web("windows-1251", &e::WINDOWS_1251_INIT),
    web("windows-1252", &e::WINDOWS_1252_INIT),
    web("windows-1253", &e::WINDOWS_1253_INIT),
    web("windows-1254", &e::WINDOWS_1254_INIT),
    web("windows-1255", &e::WINDOWS_1255_INIT),
    web("windows-1256", &e::WINDOWS_1256_INIT),
    web("windows-1257", &e::WINDOWS_1257_INIT),
    web("windows-1258", &e::WINDOWS_1258_INIT),
    web("cp874", &e::WINDOWS_874_INIT),
    web("windows-874", &e::WINDOWS_874_INIT),
    web("iso-8859-2", &e::ISO_8859_2_INIT),
    web("iso-8859-3", &e::ISO_8859_3_INIT),
    web("iso-8859-4", &e::ISO_8859_4_INIT),
    web("iso-8859-5", &e::ISO_8859_5_INIT),
    web("iso-8859-6", &e::ISO_8859_6_INIT),
    web("iso-8859-7", &e::ISO_8859_7_INIT),
    web("iso-8859-8", &e::ISO_8859_8_INIT),
    web("iso-8859-10", &e::ISO_8859_10_INIT),
    web("iso-8859-13", &e::ISO_8859_13_INIT),
    web("iso-8859-14", &e::ISO_8859_14_INIT),
    web("iso-8859-15", &e::ISO_8859_15_INIT),
    web("iso-8859-16", &e::ISO_8859_16_INIT),
    web("koi8-r", &e::KOI8_R_INIT),
    web("koi8-u", &e::KOI8_U_INIT),
    web("cp866", &e::IBM866_INIT),
    web("ibm866", &e::IBM866_INIT),
    web("macroman", &e::MACINTOSH_INIT),
    web("macintosh", &e::MACINTOSH_INIT),
    web("x-mac-cyrillic", &e::X_MAC_CYRILLIC_INIT),
    // EUC-KR is windows-949 (cp949, UHC) in encoding_rs: it reads both, and decodes exactly (RFC-0025 §2.1).
    web("euc-kr", &e::EUC_KR_INIT),
    web("cp949", &e::EUC_KR_INIT),
    web("uhc", &e::EUC_KR_INIT),
    web_inexact("sjis", &e::SHIFT_JIS_INIT),
    web_inexact("cp932", &e::SHIFT_JIS_INIT),
    web_inexact("euc-jp", &e::EUC_JP_INIT),
    web_inexact("iso-2022-jp", &e::ISO_2022_JP_INIT),
    web_inexact("cp936", &e::GBK_INIT),
    web_inexact("gb18030", &e::GB18030_INIT),
    web_inexact("big5", &e::BIG5_INIT),
    web_inexact("cp950", &e::BIG5_INIT),
];

/// The names nvim v0.12.6 folds to a canonical name (probed with `:set fenc={name}`), after
/// [`fold`]'s case and `_` folding.
static ALIASES: [(&str, &str); 51] = [
    ("utf8", "utf-8"),
    ("iso-8859-1", "latin1"),
    ("latin-1", "latin1"),
    ("ansi", "latin1"),
    ("utf16", "utf-16"),
    ("utf-16be", "utf-16"),
    ("utf16be", "utf-16"),
    ("utf16le", "utf-16le"),
    ("ucs2", "ucs-2"),
    ("ucs-2be", "ucs-2"),
    ("ucs2be", "ucs-2"),
    ("unicode", "ucs-2"),
    ("ucs2le", "ucs-2le"),
    ("ucs4", "ucs-4"),
    ("ucs-4be", "ucs-4"),
    ("ucs4be", "ucs-4"),
    ("utf-32", "ucs-4"),
    ("utf32", "ucs-4"),
    ("utf-32be", "ucs-4"),
    ("utf32be", "ucs-4"),
    ("ucs4le", "ucs-4le"),
    ("utf-32le", "ucs-4le"),
    ("utf32le", "ucs-4le"),
    ("latin2", "iso-8859-2"),
    ("latin3", "iso-8859-3"),
    ("latin4", "iso-8859-4"),
    ("cyrillic", "iso-8859-5"),
    ("arabic", "iso-8859-6"),
    ("greek", "iso-8859-7"),
    ("hebrew", "iso-8859-8"),
    ("latin6", "iso-8859-10"),
    ("nordic", "iso-8859-10"),
    ("latin7", "iso-8859-13"),
    ("latin8", "iso-8859-14"),
    ("latin9", "iso-8859-15"),
    ("mac", "macroman"),
    ("mac-roman", "macroman"),
    ("shift-jis", "sjis"),
    ("pck", "sjis"),
    ("932", "cp932"),
    ("eucjp", "euc-jp"),
    ("ujis", "euc-jp"),
    ("unix-jis", "euc-jp"),
    ("japan", "euc-jp"),
    ("euckr", "euc-kr"),
    ("5601", "euc-kr"),
    ("korea", "euc-kr"),
    ("949", "cp949"),
    ("936", "cp936"),
    ("gbk", "cp936"),
    ("950", "cp950"),
];

fn rows() -> impl Iterator<Item = &'static Row> {
    ROWS.iter()
}

/// nvim's name folding before the alias table: lower case, `_` as `-`, and `iso8859` as `iso-8859`.
fn fold(name: &str) -> String {
    let n = name.trim().to_ascii_lowercase().replace('_', "-");
    match n.strip_prefix("iso8859") {
        Some(rest) => format!("iso-8859{rest}"),
        None => n,
    }
}

/// An encoding a file is read and written in: a row of ruse's name table (RFC-0025 §2). The default is the
/// empty name, UTF-8 with invalid bytes kept: what every file was before RFC-0025.
#[derive(Clone, Copy)]
pub struct FileEncoding(&'static Row);

impl Default for FileEncoding {
    fn default() -> Self {
        FileEncoding(&ROWS[0])
    }
}

impl PartialEq for FileEncoding {
    fn eq(&self, other: &Self) -> bool {
        self.0.name == other.0.name
    }
}

impl Eq for FileEncoding {}

impl std::fmt::Debug for FileEncoding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "FileEncoding({:?})", self.0.name)
    }
}

/// A name that is not in ruse's table: `:set fenc=` and `++enc=` refuse it (`E474`, RFC-0025 §2.2).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct UnknownEncoding(pub String);

impl std::fmt::Display for UnknownEncoding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "unknown encoding: {}", self.0)
    }
}

/// A character the encoding cannot hold (`E513` on a write, RFC-0025 §4.1): where it is in the text given to
/// [`FileEncoding::encode`], and its 1-based line counted by `\n`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Unencodable {
    pub ch: char,
    pub offset: usize,
    pub line: usize,
}

/// A decoded file: its text, and the offset in the input of the first byte sequence the encoding could not
/// decode, which the text keeps as it is.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Decoded<'a> {
    pub text: Cow<'a, [u8]>,
    pub malformed: Option<usize>,
}

const BOM_UTF8: &[u8] = &[0xEF, 0xBB, 0xBF];
const BOM_UTF16BE: &[u8] = &[0xFE, 0xFF];
const BOM_UTF16LE: &[u8] = &[0xFF, 0xFE];
const BOM_UTF32BE: &[u8] = &[0x00, 0x00, 0xFE, 0xFF];
const BOM_UTF32LE: &[u8] = &[0xFF, 0xFE, 0x00, 0x00];

impl FileEncoding {
    /// The encoding called `name`, folded as nvim folds `'fileencoding'` (case, `_`, its aliases). The empty
    /// name is the default.
    pub fn from_name(name: &str) -> Result<FileEncoding, UnknownEncoding> {
        let folded = fold(name);
        let canonical = ALIASES
            .iter()
            .find(|(alias, _)| *alias == folded)
            .map_or(folded.as_str(), |(_, c)| c);
        FileEncoding::from_canonical(canonical.as_bytes())
            .ok_or_else(|| UnknownEncoding(name.to_owned()))
    }

    /// The encoding whose canonical name is exactly `name` (a journal's record, RFC-0025 §7); `None` for a
    /// name this build does not have.
    pub fn from_canonical(name: &[u8]) -> Option<FileEncoding> {
        rows().find(|r| r.name.as_bytes() == name).map(FileEncoding)
    }

    /// The canonical name; empty for the default.
    #[must_use]
    pub fn name(self) -> &'static str {
        self.0.name
    }

    /// Every canonical name, the empty one left out (for completion and `:checkhealth`).
    pub fn names() -> impl Iterator<Item = &'static str> {
        rows().skip(1).map(|r| r.name)
    }

    /// The encoding's byte order mark, which `'bomb'` writes: UTF-8's for the default and `utf-8`, and the
    /// UTF-16 and UTF-32 ones; `None` for an encoding without one.
    #[must_use]
    pub fn bom(self) -> Option<&'static [u8]> {
        match self.0.codec {
            Codec::Utf8 => Some(BOM_UTF8),
            Codec::Utf16 { be: true, .. } => Some(BOM_UTF16BE),
            Codec::Utf16 { be: false, .. } => Some(BOM_UTF16LE),
            Codec::Utf32 { be: true } => Some(BOM_UTF32BE),
            Codec::Utf32 { be: false } => Some(BOM_UTF32LE),
            Codec::Latin1 | Codec::Whatwg { .. } => None,
        }
    }

    /// The encoding a file's BOM names, as `'fileencodings'`' `ucs-bom` reads it (RFC-0025 §3): UTF-32's
    /// before UTF-16's, so `FF FE 00 00` is `ucs-4le`, then UTF-16BE and LE, then UTF-8.
    pub fn sniff_bom(raw: &[u8]) -> Option<FileEncoding> {
        let name: &[u8] = if raw.starts_with(BOM_UTF32LE) {
            b"ucs-4le"
        } else if raw.starts_with(BOM_UTF32BE) {
            b"ucs-4"
        } else if raw.starts_with(BOM_UTF16BE) {
            b"utf-16"
        } else if raw.starts_with(BOM_UTF16LE) {
            b"utf-16le"
        } else if raw.starts_with(BOM_UTF8) {
            b"utf-8"
        } else {
            return None;
        };
        FileEncoding::from_canonical(name)
    }

    /// Whether the text is the bytes (the default and `utf-8`): decoding and encoding copy nothing.
    #[must_use]
    pub fn is_utf8(self) -> bool {
        matches!(self.0.codec, Codec::Utf8)
    }

    /// `raw` (without its BOM) as text, a byte the encoding cannot decode kept as it is. Borrowed for UTF-8.
    pub fn text(self, raw: &[u8]) -> Cow<'_, [u8]> {
        if self.is_utf8() {
            Cow::Borrowed(raw)
        } else {
            self.decode(raw).text
        }
    }

    /// [`FileEncoding::text`], with where the first byte the encoding could not decode is.
    pub fn decode(self, raw: &[u8]) -> Decoded<'_> {
        match self.0.codec {
            Codec::Utf8 => Decoded {
                text: Cow::Borrowed(raw),
                malformed: std::str::from_utf8(raw).err().map(|e| e.valid_up_to()),
            },
            Codec::Latin1 => Decoded {
                text: Cow::Owned(decode_latin1(raw)),
                malformed: None,
            },
            Codec::Utf16 { be, pairs } => decode_utf16(raw, be, pairs),
            Codec::Utf32 { be } => decode_utf32(raw, be),
            Codec::Whatwg { enc, .. } => decode_whatwg(raw, enc),
        }
    }

    /// `raw` as text when the encoding reads it exactly: no byte it cannot decode, and the text encodes back
    /// to `raw`, so a buffer read in it and written unchanged gives the file back (F-008). This is what a
    /// `'fileencodings'` entry accepts (RFC-0025 §3).
    pub fn decode_exact(self, raw: &[u8]) -> Option<Cow<'_, [u8]>> {
        let d = self.decode(raw);
        if d.malformed.is_some() {
            return None;
        }
        if let Codec::Whatwg { exact: false, .. } = self.0.codec {
            if self.encode(&d.text).ok()?.as_ref() != raw {
                return None;
            }
        }
        Some(d.text)
    }

    /// `text` in this encoding (without a BOM). A byte of `text` that is not valid UTF-8 is written as it is;
    /// a character the encoding cannot hold fails the whole encode. Borrowed for UTF-8.
    pub fn encode(self, text: &[u8]) -> Result<Cow<'_, [u8]>, Unencodable> {
        if self.is_utf8() {
            return Ok(Cow::Borrowed(text));
        }
        let mut out = Vec::with_capacity(text.len());
        let mut at = 0;
        for chunk in text.utf8_chunks() {
            let valid = chunk.valid();
            self.encode_str(valid, &mut out).map_err(|(ch, i)| {
                let offset = at + i;
                Unencodable {
                    ch,
                    offset,
                    line: 1 + text[..offset].iter().filter(|&&b| b == b'\n').count(),
                }
            })?;
            out.extend_from_slice(chunk.invalid());
            at += valid.len() + chunk.invalid().len();
        }
        Ok(Cow::Owned(out))
    }

    /// Encode valid text onto `out`; on a character the encoding cannot hold, it and its offset in `s`.
    fn encode_str(self, s: &str, out: &mut Vec<u8>) -> Result<(), (char, usize)> {
        match self.0.codec {
            Codec::Utf8 => out.extend_from_slice(s.as_bytes()),
            Codec::Latin1 => {
                for (i, c) in s.char_indices() {
                    let b = u8::try_from(u32::from(c)).map_err(|_| (c, i))?;
                    out.push(b);
                }
            }
            Codec::Utf16 { be, pairs } => {
                let mut units = [0u16; 2];
                for (i, c) in s.char_indices() {
                    if !pairs && u32::from(c) > 0xFFFF {
                        return Err((c, i));
                    }
                    for u in c.encode_utf16(&mut units) {
                        out.extend_from_slice(&if be { u.to_be_bytes() } else { u.to_le_bytes() });
                    }
                }
            }
            Codec::Utf32 { be } => {
                for c in s.chars() {
                    let v = u32::from(c);
                    out.extend_from_slice(&if be { v.to_be_bytes() } else { v.to_le_bytes() });
                }
            }
            Codec::Whatwg { enc, .. } => encode_whatwg(s, enc, out)?,
        }
        Ok(())
    }
}

fn decode_latin1(raw: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(raw.len() + raw.len() / 4);
    let mut buf = [0u8; 4];
    for &b in raw {
        out.extend_from_slice(char::from(b).encode_utf8(&mut buf).as_bytes());
    }
    out
}

/// Append `c` as UTF-8.
fn push_char(out: &mut Vec<u8>, c: char) {
    let mut buf = [0u8; 4];
    out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
}

fn decode_utf16(raw: &[u8], be: bool, pairs: bool) -> Decoded<'_> {
    let unit = |i: usize| {
        let b = [raw[i], raw[i + 1]];
        if be {
            u16::from_be_bytes(b)
        } else {
            u16::from_le_bytes(b)
        }
    };
    let mut out = Vec::with_capacity(raw.len());
    let mut malformed = None;
    let mut i = 0;
    while i < raw.len() {
        if i + 1 >= raw.len() {
            // An odd final byte.
            malformed.get_or_insert(i);
            out.extend_from_slice(&raw[i..]);
            break;
        }
        let u = unit(i);
        let (c, len) = match u {
            0xD800..=0xDBFF if pairs && i + 3 < raw.len() => {
                let lo = unit(i + 2);
                if (0xDC00..=0xDFFF).contains(&lo) {
                    let v = 0x10000 + ((u32::from(u) - 0xD800) << 10) + (u32::from(lo) - 0xDC00);
                    (char::from_u32(v), 4)
                } else {
                    (None, 2)
                }
            }
            _ => (char::from_u32(u32::from(u)), 2), // a surrogate is not a char: None
        };
        match c {
            Some(c) => push_char(&mut out, c),
            None => {
                malformed.get_or_insert(i);
                out.extend_from_slice(&raw[i..i + len]);
            }
        }
        i += len;
    }
    Decoded {
        text: Cow::Owned(out),
        malformed,
    }
}

fn decode_utf32(raw: &[u8], be: bool) -> Decoded<'_> {
    let mut out = Vec::with_capacity(raw.len());
    let mut malformed = None;
    let (quads, rest) = raw.as_chunks::<4>();
    for (n, q) in quads.iter().enumerate() {
        let v = if be {
            u32::from_be_bytes(*q)
        } else {
            u32::from_le_bytes(*q)
        };
        match char::from_u32(v) {
            Some(c) => push_char(&mut out, c),
            None => {
                malformed.get_or_insert(4 * n);
                out.extend_from_slice(q);
            }
        }
    }
    if !rest.is_empty() {
        malformed.get_or_insert(raw.len() - rest.len());
        out.extend_from_slice(rest);
    }
    Decoded {
        text: Cow::Owned(out),
        malformed,
    }
}

fn decode_whatwg<'a>(raw: &'a [u8], enc: &'static Encoding) -> Decoded<'a> {
    let mut d = enc.new_decoder_without_bom_handling();
    let mut out = Vec::with_capacity(raw.len() + raw.len() / 2);
    let mut buf = [0u8; 4096];
    let mut malformed = None;
    let mut pos = 0;
    loop {
        let (r, read, written) = d.decode_to_utf8_without_replacement(&raw[pos..], &mut buf, true);
        out.extend_from_slice(&buf[..written]);
        pos += read;
        match r {
            DecoderResult::InputEmpty => break,
            DecoderResult::OutputFull => {}
            DecoderResult::Malformed(bad, after) => {
                // The malformed bytes end `after` bytes before the read position; the decoder emits those
                // later bytes itself, so the kept bytes go here, in order. The whole input is one buffer, so
                // they are in `raw`.
                let end = pos.saturating_sub(usize::from(after));
                let start = end.saturating_sub(usize::from(bad));
                malformed.get_or_insert(start);
                out.extend_from_slice(&raw[start..end]);
            }
        }
    }
    Decoded {
        text: Cow::Owned(out),
        malformed,
    }
}

/// Encode `s` with a fresh encoder that ends in its initial state (ISO-2022-JP returns to ASCII), so a kept
/// byte written after it is not read in another state.
fn encode_whatwg(s: &str, enc: &'static Encoding, out: &mut Vec<u8>) -> Result<(), (char, usize)> {
    let mut en = enc.new_encoder();
    let mut buf = [0u8; 4096];
    let mut pos = 0;
    loop {
        let (r, read, written) = en.encode_from_utf8_without_replacement(&s[pos..], &mut buf, true);
        out.extend_from_slice(&buf[..written]);
        pos += read;
        match r {
            EncoderResult::InputEmpty => return Ok(()),
            EncoderResult::OutputFull => {}
            EncoderResult::Unmappable(c) => return Err((c, pos - c.len_utf8())),
        }
    }
}

#[cfg(test)]
mod tests;
