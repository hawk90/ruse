//! The diagnostics list picker overlay (F-014): list the focused buffer's diagnostics, filter by a typed
//! query, and on Enter jump to the diagnostic's byte offset. A [`Picker`]`<usize>` whose payload is the
//! diagnostic's start byte; the accept action (`ws.place_focused_cursor`) lives in `app::session`.

use crate::lsp::model::Severity;
use crate::lsp::Diag;
use crate::ui::picker::{PickItem, Picker};

/// Open a diagnostics picker over `diags` (byte ranges into `bytes`). Each row shows
/// `line:col [E/W/I/H] message` (1-based, first message line); the payload is the diagnostic's start byte.
pub(crate) fn open(diags: &[Diag], bytes: &[u8]) -> Picker<usize> {
    let items = diags
        .iter()
        .map(|d| {
            let (line, col) = line_col(bytes, d.start);
            let sev = match d.severity {
                Severity::Error => 'E',
                Severity::Warning => 'W',
                Severity::Info => 'I',
                Severity::Hint => 'H',
            };
            let msg = d.message.lines().next().unwrap_or("");
            let display = format!("{line}:{col} [{sev}] {msg}");
            PickItem {
                search: display.clone(),
                display,
                payload: d.start,
            }
        })
        .collect();
    Picker::new(items)
}

/// 1-based `(line, column)` of byte offset `off` in `bytes` (column counts bytes from the line start — good
/// enough for a picker label; the jump uses the exact byte offset).
fn line_col(bytes: &[u8], off: usize) -> (usize, usize) {
    let off = off.min(bytes.len());
    let line = bytes[..off].iter().filter(|&&b| b == b'\n').count() + 1;
    let line_start = bytes[..off]
        .iter()
        .rposition(|&b| b == b'\n')
        .map_or(0, |p| p + 1);
    (line, off - line_start + 1)
}

#[cfg(test)]
mod diag_picker_tests {
    use super::*;

    fn diag(start: usize, end: usize, severity: Severity, message: &str) -> Diag {
        Diag {
            start,
            end,
            severity,
            message: message.to_string(),
        }
    }

    #[test]
    fn rows_show_line_col_severity_and_payload_is_offset() {
        let bytes = b"fn main() {\n    let x = 1\n}\n"; // line 2 starts at byte 12
        let diags = vec![
            diag(3, 7, Severity::Warning, "unused"),
            diag(24, 24, Severity::Error, "expected `;`\nsecond line ignored"),
        ];
        let p = open(&diags, bytes);
        let rows: Vec<String> = p.rows().into_iter().map(|(r, _)| r).collect();
        assert_eq!(rows[0], "1:4 [W] unused");
        assert_eq!(rows[1], "2:13 [E] expected `;`"); // only the first message line
        assert_eq!(p.selected(), Some(&3)); // payload = start byte
    }
}
