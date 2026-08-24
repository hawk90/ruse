//! Pure builders for the Normal-mode INFO commands that print to the status line without mutating the
//! buffer: `ga`/`:ascii` (character value), `CTRL-G` (file info), and `g CTRL-G` (cursor position/counts).
//!
//! The input engine has no buffer, so — like `*`/`gd` — the frontend resolves these against the focused
//! buffer's bytes and cursor, then formats the message here. Every format below was captured HEADLESSLY
//! from Neovim v0.12.4 (`nvim -u NONE --headless … -c 'redir >…' -c ascii` / `execute "normal! …"`) and is
//! matched byte-for-byte where practical; the few documented divergences are noted at each helper.
//!
//! Byte/char/line totals follow Vim's buffer model: every line contributes exactly one line-ending, so a
//! buffer stored WITHOUT a trailing newline (`noeol`) still counts a phantom EOL (`+1` byte and `+1` char),
//! exactly as Vim's `g CTRL-G` reports.

use crate::motion::{next_boundary, vcol_of};
use crate::pos::{line_end, line_of, line_start};

/// Reverse digraph table (codepoint → Vim digraph name) for the `Digr` field of `ga`/`:ascii`, covering
/// the C0 controls, space, ASCII punctuation, and the Latin-1 supplement (codepoints `<= 0xFF`) — the
/// range a cursor realistically lands on. Mechanically extracted from `digraph_getlist(1)` on Neovim
/// v0.12.4 (first-occurrence wins, matching nvim's `get_digraph_for_char` table scan) and validated by
/// sweeping `:ascii` over the range (0 mismatches). Sorted by codepoint for binary search.
///
/// DIVERGENCE: codepoints `> 0xFF` that have a digraph in nvim's full ~1300-entry table (arrows, currency,
/// CJK punctuation, …) are omitted here, so `ga` prints the no-digraph form (`Octal …`, no `Digr`) for
/// them. This mirrors ruse's deliberately curated `i_CTRL-K` digraph subset (the full table is a follow-up)
/// and keeps this data proportionate; the common accented-Latin and whitespace/punctuation cases are exact.
#[rustfmt::skip]
const DIGRAPHS: &[(u32, &str)] = &[
    (0x01, "SH"), (0x02, "SX"), (0x03, "EX"), (0x04, "ET"), (0x05, "EQ"), (0x06, "AK"),
    (0x07, "BL"), (0x08, "BS"), (0x09, "HT"), (0x0A, "NU"), (0x0B, "VT"), (0x0C, "FF"),
    (0x0D, "CR"), (0x0E, "SO"), (0x0F, "SI"), (0x10, "DL"), (0x11, "D1"), (0x12, "D2"),
    (0x13, "D3"), (0x14, "D4"), (0x15, "NK"), (0x16, "SY"), (0x17, "EB"), (0x18, "CN"),
    (0x19, "EM"), (0x1A, "SB"), (0x1B, "EC"), (0x1C, "FS"), (0x1D, "GS"), (0x1E, "RS"),
    (0x1F, "US"), (0x20, "SP"), (0x23, "Nb"), (0x24, "DO"), (0x40, "At"), (0x5B, "<("),
    (0x5C, "//"), (0x5D, ")>"), (0x5E, "'>"), (0x60, "'!"), (0x7B, "(!"), (0x7C, "!!"),
    (0x7D, "!)"), (0x7E, "'?"), (0x7F, "DT"), (0x80, "PA"), (0x81, "HO"), (0x82, "BH"),
    (0x83, "NH"), (0x84, "IN"), (0x85, "NL"), (0x86, "SA"), (0x87, "ES"), (0x88, "HS"),
    (0x89, "HJ"), (0x8A, "VS"), (0x8B, "PD"), (0x8C, "PU"), (0x8D, "RI"), (0x8E, "S2"),
    (0x8F, "S3"), (0x90, "DC"), (0x91, "P1"), (0x92, "P2"), (0x93, "TS"), (0x94, "CC"),
    (0x95, "MW"), (0x96, "SG"), (0x97, "EG"), (0x98, "SS"), (0x99, "GC"), (0x9A, "SC"),
    (0x9B, "CI"), (0x9C, "ST"), (0x9D, "OC"), (0x9E, "PM"), (0x9F, "AC"), (0xA0, "NS"),
    (0xA1, "!I"), (0xA2, "Ct"), (0xA3, "Pd"), (0xA4, "Cu"), (0xA5, "Ye"), (0xA6, "BB"),
    (0xA7, "SE"), (0xA8, "':"), (0xA9, "Co"), (0xAA, "-a"), (0xAB, "<<"), (0xAC, "NO"),
    (0xAD, "--"), (0xAE, "Rg"), (0xAF, "'m"), (0xB0, "DG"), (0xB1, "+-"), (0xB2, "2S"),
    (0xB3, "3S"), (0xB4, "''"), (0xB5, "My"), (0xB6, "PI"), (0xB7, ".M"), (0xB8, "',"),
    (0xB9, "1S"), (0xBA, "-o"), (0xBB, ">>"), (0xBC, "14"), (0xBD, "12"), (0xBE, "34"),
    (0xBF, "?I"), (0xC0, "A!"), (0xC1, "A'"), (0xC2, "A>"), (0xC3, "A?"), (0xC4, "A:"),
    (0xC5, "AA"), (0xC6, "AE"), (0xC7, "C,"), (0xC8, "E!"), (0xC9, "E'"), (0xCA, "E>"),
    (0xCB, "E:"), (0xCC, "I!"), (0xCD, "I'"), (0xCE, "I>"), (0xCF, "I:"), (0xD0, "D-"),
    (0xD1, "N?"), (0xD2, "O!"), (0xD3, "O'"), (0xD4, "O>"), (0xD5, "O?"), (0xD6, "O:"),
    (0xD7, "*X"), (0xD8, "O/"), (0xD9, "U!"), (0xDA, "U'"), (0xDB, "U>"), (0xDC, "U:"),
    (0xDD, "Y'"), (0xDE, "TH"), (0xDF, "ss"), (0xE0, "a!"), (0xE1, "a'"), (0xE2, "a>"),
    (0xE3, "a?"), (0xE4, "a:"), (0xE5, "aa"), (0xE6, "ae"), (0xE7, "c,"), (0xE8, "e!"),
    (0xE9, "e'"), (0xEA, "e>"), (0xEB, "e:"), (0xEC, "i!"), (0xED, "i'"), (0xEE, "i>"),
    (0xEF, "i:"), (0xF0, "d-"), (0xF1, "n?"), (0xF2, "o!"), (0xF3, "o'"), (0xF4, "o>"),
    (0xF5, "o?"), (0xF6, "o:"), (0xF7, "-:"), (0xF8, "o/"), (0xF9, "u!"), (0xFA, "u'"),
    (0xFB, "u>"), (0xFC, "u:"), (0xFD, "y'"), (0xFE, "th"), (0xFF, "y:"),
];

/// The Vim digraph name for codepoint `cp`, or `None` when it has none in the curated `<= 0xFF` range.
#[must_use]
pub fn digraph_name(cp: u32) -> Option<&'static str> {
    DIGRAPHS
        .binary_search_by_key(&cp, |&(c, _)| c)
        .ok()
        .map(|i| DIGRAPHS[i].1)
}

/// The character under the cursor, or `None` for an empty line / end-of-buffer (Vim reports `NUL` there).
/// A `\n` under the cursor (an empty line) is treated as no character.
fn char_under(bytes: &[u8], cursor: usize) -> Option<char> {
    if cursor >= bytes.len() || bytes[cursor] == b'\n' {
        return None;
    }
    let end = next_boundary(bytes, cursor);
    match std::str::from_utf8(&bytes[cursor..end]) {
        Ok(s) => s.chars().next(),
        // Invalid UTF-8 (e.g. a lone high byte): fall back to the raw byte as a codepoint, matching Vim's
        // best-effort display of an isolated byte.
        Err(_) => Some(char::from(bytes[cursor])),
    }
}

/// The printable form Vim shows between `<…>` for an ASCII codepoint: `^X` caret notation for the C0
/// controls, `^?` for DEL, and the literal glyph (including a space) otherwise.
fn ascii_transchar(cp: u32) -> String {
    match cp {
        0x7F => "^?".to_string(),
        c if c < 0x20 => format!("^{}", char::from(c as u8 + 0x40)),
        c => char::from(c as u8).to_string(),
    }
}

/// `ga` / `:ascii` / `:as` — the numeric value of the character under the cursor, matching Neovim v0.12.4:
/// - ASCII (`< 0x80`): `<H>  72,  Hex 48,  Octal 110`, or `< >  32,  Hex 20,  Oct 040, Digr SP` when a
///   digraph exists (the word switches to `Oct` and the `Digr` field is appended, exactly as Vim does).
/// - Multibyte (`>= 0x80`): `<é> 233, Hex 00e9, Oct 351, Digr e'` (Hex is 4 wide, or 8 above `U+FFFF`).
/// - Empty line / end-of-buffer: `NUL`.
///
/// DIVERGENCE: a base char with combining marks is reported for its base codepoint only (Vim can list the
/// composing codepoints too); and see [`DIGRAPHS`] for the `> 0xFF` digraph omission.
#[must_use]
pub fn ascii_info(bytes: &[u8], cursor: usize) -> String {
    let Some(c) = char_under(bytes, cursor) else {
        return "NUL".to_string();
    };
    let cp = c as u32;
    let digr = digraph_name(cp);
    if cp < 0x80 {
        let trans = ascii_transchar(cp);
        match digr {
            Some(d) => format!("<{trans}>  {cp},  Hex {cp:02x},  Oct {cp:03o}, Digr {d}"),
            None => format!("<{trans}>  {cp},  Hex {cp:02x},  Octal {cp:03o}"),
        }
    } else {
        let hex = if cp <= 0xFFFF {
            format!("{cp:04x}")
        } else {
            format!("{cp:08x}")
        };
        match digr {
            Some(d) => format!("<{c}> {cp}, Hex {hex}, Oct {cp:o}, Digr {d}"),
            None => format!("<{c}> {cp}, Hex {hex}, Octal {cp:o}"),
        }
    }
}

/// The number of lines in `bytes`, Vim-style: one line per `\n`, plus a trailing line when the buffer does
/// not end in `\n` (`noeol`). An empty buffer has zero lines.
fn line_count(bytes: &[u8]) -> usize {
    if bytes.is_empty() {
        return 0;
    }
    let nl = bytes.iter().filter(|&&b| b == b'\n').count();
    if bytes.last() == Some(&b'\n') {
        nl
    } else {
        nl + 1
    }
}

/// `CTRL-G` (Normal) — file info, matching Neovim v0.12.4: `"foo.txt" 5 lines --60%--`, with ` [Modified]`
/// after the name when the buffer has unsaved edits, `"[No Name]"` for an unnamed buffer, and
/// `"foo.txt" --No lines in buffer--` for an empty buffer. `name` is the display name the frontend owns
/// (`None` → `[No Name]`); the percentage is `cursor_line * 100 / total_lines` (integer), as Vim computes.
#[must_use]
pub fn file_info(name: Option<&str>, modified: bool, bytes: &[u8], cursor: usize) -> String {
    let name = name.unwrap_or("[No Name]");
    let lines = line_count(bytes);
    if lines == 0 {
        return format!("\"{name}\" --No lines in buffer--");
    }
    let modflag = if modified { " [Modified]" } else { "" };
    let word = if lines == 1 { "line" } else { "lines" };
    let cur_line = (line_of(bytes, cursor) + 1).min(lines);
    let pct = cur_line * 100 / lines;
    format!("\"{name}\"{modflag} {lines} {word} --{pct}%--")
}

/// Number of Unicode scalar values in `s` (falling back to the byte length for invalid UTF-8).
fn char_count(s: &[u8]) -> usize {
    std::str::from_utf8(s).map_or(s.len(), |t| t.chars().count())
}

/// Vim's `col_print`: one number when the byte column and virtual column coincide, else `byte-vcol`.
fn col_print(col: usize, vcol: usize) -> String {
    if col == vcol {
        col.to_string()
    } else {
        format!("{col}-{vcol}")
    }
}

/// `g CTRL-G` — cursor position and buffer counts, matching Neovim v0.12.4:
/// `Col 3 of 10; Line 3 of 5; Word 7 of 12; Byte 36 of 66`, with a `Char c of t` field inserted before
/// `Byte` whenever the char count differs from the byte count (i.e. multibyte text is present), and each
/// `Col` side rendered as `byte-vcol` when tabs/multibyte make them differ. An empty buffer yields
/// `--No lines in buffer--`.
///
/// Words are whitespace-separated runs (Vim's `g CTRL-G` definition — punctuation does NOT split, unlike
/// `w`): `foo,bar baz` is two words. A cursor on leading whitespace reports `Word 0`. Byte/Char totals
/// include one line-ending per line (see the module docs).
///
/// DIVERGENCE: virtual columns treat every non-tab glyph as one cell, so a double-width (CJK/emoji) glyph
/// under the cursor is off by its extra cells in the `Col` vcol field; tabs and Latin/accented text are
/// exact. `tabstop` sizes `<Tab>` expansion.
#[must_use]
pub fn cursor_pos_info(bytes: &[u8], cursor: usize, tabstop: usize) -> String {
    if line_count(bytes) == 0 {
        return "--No lines in buffer--".to_string();
    }
    let cursor = cursor.min(bytes.len());
    let start = line_start(bytes, cursor);
    let lend = line_end(bytes, cursor);

    // Column: 1-based byte offset within the line, and the cursor's virtual column (the LAST cell of the
    // char under the cursor — Vim's `virtcol('.')`, so a Tab reports its final cell).
    let bytecol = cursor - start + 1;
    let vcol = if cursor < lend {
        vcol_of(bytes, start, next_boundary(bytes, cursor), tabstop)
    } else {
        bytecol // empty line / cursor at line end: one cell.
    };
    let line_bytes = lend - start;
    let line_vcols = vcol_of(bytes, start, lend, tabstop);

    let cur_line = line_of(bytes, cursor) + 1;
    let total_lines = line_count(bytes);

    // Word count (whitespace-separated) over the whole buffer, recording the count at the cursor AFTER any
    // word started by the cursor's own char (so the first char of a word reads as that word's number, and a
    // cursor on whitespace reads as the number of words fully seen so far — `0` on leading whitespace).
    let mut word_count = 0usize;
    let mut word_cursor = 0usize;
    let mut in_word = false;
    for (i, &b) in bytes.iter().enumerate() {
        if b.is_ascii_whitespace() {
            in_word = false;
        } else if !in_word {
            word_count += 1;
            in_word = true;
        }
        if i == cursor {
            word_cursor = word_count;
        }
    }
    if cursor >= bytes.len() {
        word_cursor = word_count;
    }

    let noeol = usize::from(bytes.last() != Some(&b'\n'));
    let byte_cursor = cursor + 1;
    let byte_total = bytes.len() + noeol;
    let char_cursor = char_count(&bytes[..cursor]) + 1;
    let char_total = char_count(bytes) + noeol;

    let head = format!(
        "Col {} of {}; Line {cur_line} of {total_lines}; Word {word_cursor} of {word_count}",
        col_print(bytecol, vcol),
        col_print(line_bytes, line_vcols),
    );
    if char_cursor != byte_cursor || char_total != byte_total {
        format!("{head}; Char {char_cursor} of {char_total}; Byte {byte_cursor} of {byte_total}")
    } else {
        format!("{head}; Byte {byte_cursor} of {byte_total}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- ga / :ascii ---------------------------------------------------------------------------------

    #[test]
    fn ascii_printable_no_digraph() {
        // `<H>  72,  Hex 48,  Octal 110` — capital H has no digraph, so the full word `Octal` and no `Digr`.
        assert_eq!(ascii_info(b"Hello", 0), "<H>  72,  Hex 48,  Octal 110");
        assert_eq!(ascii_info(b"a", 0), "<a>  97,  Hex 61,  Octal 141");
    }

    #[test]
    fn ascii_with_digraph_switches_to_oct_and_appends_digr() {
        // Space and Tab have digraphs (SP/HT): the field abbreviates to `Oct` and appends `Digr`.
        assert_eq!(ascii_info(b"a b", 1), "< >  32,  Hex 20,  Oct 040, Digr SP");
        assert_eq!(ascii_info(b"\t", 0), "<^I>  9,  Hex 09,  Oct 011, Digr HT");
        assert_eq!(ascii_info(b"~", 0), "<~>  126,  Hex 7e,  Oct 176, Digr '?");
    }

    #[test]
    fn ascii_multibyte_latin1() {
        // `<é> 233, Hex 00e9, Oct 351, Digr e'` — 2-byte é, 4-wide hex, digraph present.
        assert_eq!(
            ascii_info("é".as_bytes(), 0),
            "<é> 233, Hex 00e9, Oct 351, Digr e'"
        );
        assert_eq!(
            ascii_info("ÿ".as_bytes(), 0),
            "<ÿ> 255, Hex 00ff, Oct 377, Digr y:"
        );
    }

    #[test]
    fn ascii_multibyte_beyond_latin1_has_no_digraph() {
        // CJK / emoji: no digraph in the curated table → `Octal`; emoji hex widens to 8.
        assert_eq!(
            ascii_info("中".as_bytes(), 0),
            "<中> 20013, Hex 4e2d, Octal 47055"
        );
        assert_eq!(
            ascii_info("😀".as_bytes(), 0),
            "<😀> 128512, Hex 0001f600, Octal 373000"
        );
    }

    #[test]
    fn ascii_empty_line_and_eob_report_nul() {
        assert_eq!(ascii_info(b"", 0), "NUL");
        assert_eq!(ascii_info(b"\n", 0), "NUL"); // cursor on the newline of an empty line.
        assert_eq!(ascii_info(b"ab\n", 3), "NUL"); // past end.
    }

    // --- CTRL-G --------------------------------------------------------------------------------------

    #[test]
    fn file_info_named_plural_and_percent() {
        let b = b"Hello world foo\nsecond line here\nthird line\nfourth\nfifth line end\n";
        // Cursor on line 1 -> 20%; a byte on line 5 (the 'd' of "end", just before the final \n) -> 100%.
        assert_eq!(
            file_info(Some("f5.txt"), false, b, 0),
            "\"f5.txt\" 5 lines --20%--"
        );
        assert_eq!(
            file_info(Some("f5.txt"), false, b, b.len() - 2),
            "\"f5.txt\" 5 lines --100%--"
        );
    }

    #[test]
    fn file_info_modified_flag_and_singular() {
        assert_eq!(
            file_info(Some("f5.txt"), true, b"only\n", 0),
            "\"f5.txt\" [Modified] 1 line --100%--"
        );
    }

    #[test]
    fn file_info_unnamed_and_empty() {
        assert_eq!(
            file_info(None, false, b"x\n", 0),
            "\"[No Name]\" 1 line --100%--"
        );
        assert_eq!(
            file_info(None, false, b"", 0),
            "\"[No Name]\" --No lines in buffer--"
        );
        assert_eq!(
            file_info(Some("e.txt"), false, b"", 0),
            "\"e.txt\" --No lines in buffer--"
        );
    }

    #[test]
    fn file_info_noeol_counts_the_line() {
        assert_eq!(
            file_info(Some("n.txt"), false, b"abc", 0),
            "\"n.txt\" 1 line --100%--"
        );
    }

    // --- g CTRL-G ------------------------------------------------------------------------------------

    #[test]
    fn cursor_pos_ascii_midbuffer() {
        // Neovim: `Col 3 of 10; Line 3 of 5; Word 7 of 12; Byte 36 of 66` (cursor on 'i' of "third").
        let b = b"Hello world foo\nsecond line here\nthird line\nfourth\nfifth line end\n";
        let cursor = b"Hello world foo\nsecond line here\nth".len(); // byte 35 -> 'i'
        assert_eq!(
            cursor_pos_info(b, cursor, 8),
            "Col 3 of 10; Line 3 of 5; Word 7 of 12; Byte 36 of 66"
        );
    }

    #[test]
    fn cursor_pos_words_are_whitespace_separated() {
        // Punctuation does NOT split words (unlike `w`): "foo,bar baz.qux" is 2 words.
        assert_eq!(
            cursor_pos_info(b"foo,bar baz.qux\n", 0, 8),
            "Col 1 of 15; Line 1 of 1; Word 1 of 2; Byte 1 of 16"
        );
    }

    #[test]
    fn cursor_pos_word_zero_on_leading_whitespace() {
        assert_eq!(
            cursor_pos_info(b"  leading spaces\n", 0, 8),
            "Col 1 of 16; Line 1 of 1; Word 0 of 2; Byte 1 of 17"
        );
    }

    #[test]
    fn cursor_pos_tab_expands_vcol() {
        // "a\tbc": Col byte 2 (the Tab) sits at vcol 8; the line is 4 bytes / 10 vcols.
        assert_eq!(
            cursor_pos_info(b"a\tbc\n", 1, 8),
            "Col 2-8 of 4-10; Line 1 of 1; Word 1 of 2; Byte 2 of 5"
        );
    }

    #[test]
    fn cursor_pos_multibyte_adds_char_field() {
        // "café x": cursor on the space (byte 5). Char field appears; Col shows byte-vcol.
        assert_eq!(
            cursor_pos_info("café x\n".as_bytes(), 5, 8),
            "Col 6-5 of 7-6; Line 1 of 1; Word 1 of 2; Char 5 of 7; Byte 6 of 8"
        );
    }

    #[test]
    fn cursor_pos_noeol_counts_phantom_eol() {
        // "abc" (no trailing newline): total bytes count the phantom EOL -> 4.
        assert_eq!(
            cursor_pos_info(b"abc", 2, 8),
            "Col 3 of 3; Line 1 of 1; Word 1 of 1; Byte 3 of 4"
        );
    }

    #[test]
    fn cursor_pos_empty_buffer() {
        assert_eq!(cursor_pos_info(b"", 0, 8), "--No lines in buffer--");
    }

    #[test]
    fn digraph_table_is_sorted_for_binary_search() {
        assert!(DIGRAPHS.windows(2).all(|w| w[0].0 < w[1].0));
        assert_eq!(digraph_name(0x20), Some("SP"));
        assert_eq!(digraph_name('é' as u32), Some("e'"));
        assert_eq!(digraph_name('中' as u32), None); // beyond the curated range.
    }
}
