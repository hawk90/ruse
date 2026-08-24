//! The `:registers` viewer overlay (F-029 / F-026): list the non-empty registers with a readable preview,
//! so a recorded macro or a yank can be inspected. A [`Picker`]`<char>` whose payload is the register name;
//! it is VIEW-ONLY (Vim's `:reg` does not act on a selection), so the session just closes it on Enter.

use crate::ui::picker::{PickItem, Picker};
use ruse_core::RegKind;

/// Vim's `:reg` Type column: charwise `c`, linewise `l`, blockwise `b`.
fn type_char(kind: RegKind) -> char {
    match kind {
        RegKind::Charwise => 'c',
        RegKind::Linewise => 'l',
        RegKind::Blockwise => 'b',
    }
}

/// Render register bytes as a one-line preview: printable chars as-is, control bytes in caret notation
/// (`Esc`→`^[`, `CR`→`^M`, `Tab`→`^I`), so a macro like `iZ<Esc>` reads `iZ^[`. The navigation-key prefix
/// (`0x80`, from the macro codec) shows as `<>`. Long values are truncated with an ellipsis.
fn preview(bytes: &[u8]) -> String {
    const MAX: usize = 60;
    let mut s = String::new();
    for ch in String::from_utf8_lossy(bytes).chars() {
        match ch {
            '\u{7f}' => s.push_str("^?"),
            '\u{80}' => s.push_str("<>"), // the macro codec's navigation-key prefix (lossy-decoded)
            c if (c as u32) < 0x20 => {
                s.push('^');
                s.push((c as u8 ^ 0x40) as char); // caret notation: ^[ ^M ^I …
            }
            c => s.push(c),
        }
        if s.len() >= MAX {
            s.push('…');
            break;
        }
    }
    s
}

/// Open a register viewer over a `(name, kind, bytes)` snapshot (see `Workspace::register_snapshot`). Each
/// row mirrors Vim's `:reg` columns — `{type}  "{name}   {preview}` (type = `c`/`l`/`b`) — and is searchable
/// by name + preview text. Empty snapshot ⇒ an empty picker (shows nothing).
pub(crate) fn open(snapshot: Vec<(char, RegKind, Vec<u8>)>) -> Picker<char> {
    let items = snapshot
        .into_iter()
        .map(|(name, kind, bytes)| {
            let body = preview(&bytes);
            PickItem {
                display: format!("{}  \"{name}   {body}", type_char(kind)),
                search: format!("{name} {body}"),
                payload: name,
            }
        })
        .collect();
    Picker::new(items)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preview_shows_control_bytes_in_caret_notation() {
        assert_eq!(preview(b"iZ\x1b"), "iZ^[", "Esc as ^[");
        assert_eq!(preview(b"dd"), "dd");
        assert_eq!(preview(b"a\rb\t"), "a^Mb^I", "CR as ^M, Tab as ^I");
    }

    #[test]
    fn open_lists_each_register_with_type_column() {
        let p = open(vec![
            ('a', RegKind::Charwise, b"dd".to_vec()),
            ('"', RegKind::Linewise, b"hello\n".to_vec()),
        ]);
        assert_eq!(p.rows().len(), 2);
        // Vim `:reg` layout: `{type}  "{name}   {preview}`; newlines render as ^J.
        assert_eq!(
            p.rows()[0].0,
            "c  \"a   dd",
            "charwise register a → type `c`; got {:?}",
            p.rows()
        );
        assert_eq!(
            p.rows()[1].0,
            "l  \"\"   hello^J",
            "linewise unnamed register → type `l`; got {:?}",
            p.rows()
        );
    }
}
