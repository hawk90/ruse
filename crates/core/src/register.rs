//! The v0 register — one unnamed slot carrying yanked/deleted text plus its *type* (charwise vs linewise),
//! which governs paste geometry. This is the deliberately-minimal core of D-026: named slots (`"a`–`"z`),
//! the numbered delete-ring, and the Emacs kill-ring are **deferred** (see `docs/design/register-model.md`).
//! Only the paste-geometry semantics ship now, because that is the part that is hard to retrofit; extra
//! addressable slots are purely additive over this type.

/// A captured span of text and whether it was taken linewise (whole lines) or charwise (a partial span).
/// Linewise content is normalized to end with a newline so paste geometry is uniform.
#[derive(Clone, Default, PartialEq, Eq, Debug)]
pub struct Register {
    text: Vec<u8>,
    linewise: bool,
}

impl Register {
    /// A charwise register (a partial-line span: `yw`, `x`, charwise `d`).
    #[must_use]
    pub fn charwise(text: Vec<u8>) -> Register {
        Register {
            text,
            linewise: false,
        }
    }

    /// A linewise register (`yy`, `dd`, `Motion::Line` operators). The content is normalized to end with a
    /// trailing newline so a paste always lands as a whole line regardless of how the source line ended.
    #[must_use]
    pub fn linewise(mut text: Vec<u8>) -> Register {
        if text.last() != Some(&b'\n') {
            text.push(b'\n');
        }
        Register {
            text,
            linewise: true,
        }
    }

    /// The stored bytes.
    #[must_use]
    pub fn text(&self) -> &[u8] {
        &self.text
    }

    /// Whether the register holds whole lines (governs paste geometry).
    #[must_use]
    pub fn is_linewise(&self) -> bool {
        self.linewise
    }

    /// Whether the register is empty (nothing has been yanked or deleted yet — paste is a no-op).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linewise_normalizes_trailing_newline() {
        // A last-line yank with no trailing newline is still stored as a whole line.
        let r = Register::linewise(b"fn main() {}".to_vec());
        assert!(r.is_linewise());
        assert_eq!(r.text(), b"fn main() {}\n");
    }

    #[test]
    fn linewise_keeps_existing_newline() {
        let r = Register::linewise(b"let x = 1;\n".to_vec());
        assert_eq!(r.text(), b"let x = 1;\n", "does not double the newline");
    }

    #[test]
    fn charwise_is_verbatim() {
        let r = Register::charwise(b"value".to_vec());
        assert!(!r.is_linewise());
        assert_eq!(r.text(), b"value");
    }

    #[test]
    fn default_is_empty() {
        assert!(Register::default().is_empty());
    }
}
