//! The normalized Language Service model (F-014) — what the UI reads INSTEAD of raw LSP protocol
//! (acceptance: "Client UI never consumes raw LSP protocol"). A [`Diag`] is a byte-range diagnostic; the
//! protocol layer (`super::protocol`) converts LSP `publishDiagnostics` into these via [`lsp_pos_to_byte`].

/// Diagnostic severity, normalized from the LSP 1..=4 scale.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Severity {
    Error,
    Warning,
    Info,
    Hint,
}

impl Severity {
    /// Map the LSP `severity` field (1=Error … 4=Hint; absent defaults to Error, as most servers intend).
    pub fn from_lsp(n: Option<u8>) -> Severity {
        match n {
            Some(2) => Severity::Warning,
            Some(3) => Severity::Info,
            Some(4) => Severity::Hint,
            _ => Severity::Error,
        }
    }
}

/// One diagnostic as a byte range in the buffer plus its severity and message — the UI-facing shape.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Diag {
    pub start: usize,
    pub end: usize,
    pub severity: Severity,
    pub message: String,
}

/// `(errors, warnings)` counts for the status line.
pub fn counts(diags: &[Diag]) -> (usize, usize) {
    let errors = diags
        .iter()
        .filter(|d| d.severity == Severity::Error)
        .count();
    let warnings = diags
        .iter()
        .filter(|d| d.severity == Severity::Warning)
        .count();
    (errors, warnings)
}

/// Convert an LSP `(line, character)` position to a byte offset in `bytes`. LSP positions are **UTF-16**:
/// `line` is 0-based; `character` counts UTF-16 code units from the line start (an astral-plane char like an
/// emoji is 2 units). Clamps to the line end when `character` runs past it, and to EOF for a missing line.
pub fn lsp_pos_to_byte(bytes: &[u8], line: u32, character: u32) -> usize {
    // Walk to the start of `line`.
    let mut idx = 0usize;
    let mut cur_line = 0u32;
    while cur_line < line {
        match bytes[idx..].iter().position(|&b| b == b'\n') {
            Some(p) => {
                idx += p + 1;
                cur_line += 1;
            }
            None => return bytes.len(), // the requested line is past EOF
        }
    }
    // Walk UTF-16 units across the line's characters to the target column.
    let rest = std::str::from_utf8(&bytes[idx..]).unwrap_or("");
    let mut units = 0u32;
    for (off, ch) in rest.char_indices() {
        if ch == '\n' || units >= character {
            return idx + off;
        }
        units += ch.len_utf16() as u32;
    }
    idx + rest.len()
}

/// Convert a byte offset to an LSP `(line, character)` position — the inverse of [`lsp_pos_to_byte`]. `line`
/// is 0-based; `character` counts UTF-16 code units from the line start. Used to send the cursor position on
/// hover / goto requests.
pub fn byte_to_lsp_pos(bytes: &[u8], byte: usize) -> (u32, u32) {
    let byte = byte.min(bytes.len());
    let line = bytes[..byte].iter().filter(|&&b| b == b'\n').count() as u32;
    let line_start = bytes[..byte]
        .iter()
        .rposition(|&b| b == b'\n')
        .map_or(0, |i| i + 1);
    let seg = std::str::from_utf8(&bytes[line_start..byte]).unwrap_or("");
    let character = seg.chars().map(|c| c.len_utf16() as u32).sum();
    (line, character)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn byte_pos_round_trips_with_lsp_pos() {
        // Round-trip byte → (line,char) → byte at char boundaries across ASCII / multibyte / astral.
        let b = "let x = é;\n한 z\n😀!".as_bytes();
        for &byte in &[0usize, 3, 8, 10, 11, 15] {
            // Only check byte offsets that sit on a char boundary.
            if b.get(byte).is_none_or(|&c| c & 0xC0 != 0x80) {
                let (l, c) = byte_to_lsp_pos(b, byte);
                assert_eq!(lsp_pos_to_byte(b, l, c), byte, "round-trip at byte {byte}");
            }
        }
    }

    #[test]
    fn pos_maps_ascii_and_multibyte_and_astral() {
        // "let x = é;\n😀 next" — line 0 has a 2-byte é; line 1 starts with a 4-byte / 2-utf16-unit emoji.
        let b = "let x = é;\n😀 next".as_bytes();
        assert_eq!(lsp_pos_to_byte(b, 0, 0), 0); // start
        assert_eq!(lsp_pos_to_byte(b, 0, 8), 8); // the é is at utf16 col 8 (byte 8)
                                                 // After é (1 utf16 unit): col 9 → the ';' just after é. é is 2 bytes, so byte 10.
        assert_eq!(lsp_pos_to_byte(b, 0, 9), 10);
        // Line 1 col 0 = the emoji's first byte (right after the '\n').
        let nl = b.iter().position(|&c| c == b'\n').unwrap();
        assert_eq!(lsp_pos_to_byte(b, 1, 0), nl + 1);
        // The emoji is 2 UTF-16 units → col 2 lands just after it (the space), byte nl+1+4.
        assert_eq!(lsp_pos_to_byte(b, 1, 2), nl + 1 + 4);
    }

    #[test]
    fn pos_clamps_past_line_end_and_eof() {
        let b = b"ab\ncd";
        assert_eq!(lsp_pos_to_byte(b, 0, 99), 2); // clamp to end of line 0 (the '\n')
        assert_eq!(lsp_pos_to_byte(b, 5, 0), b.len()); // line past EOF → buffer end
    }

    #[test]
    fn counts_split_by_severity() {
        let d = |sev| Diag {
            start: 0,
            end: 1,
            severity: sev,
            message: String::new(),
        };
        let v = vec![d(Severity::Error), d(Severity::Warning), d(Severity::Error)];
        assert_eq!(counts(&v), (2, 1));
    }
}
