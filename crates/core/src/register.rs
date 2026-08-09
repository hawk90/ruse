//! The v0 register — one unnamed slot carrying yanked/deleted text plus its *type* (charwise vs linewise),
//! which governs paste geometry. This is the deliberately-minimal core of D-026: named slots (`"a`–`"z`),
//! the numbered delete-ring, and the Emacs kill-ring are **deferred** (see `docs/design/register-model.md`).
//! Only the paste-geometry semantics ship now, because that is the part that is hard to retrofit; extra
//! addressable slots are purely additive over this type.

/// The paste GEOMETRY a register carries — the one dimension that governs how a paste lands.
/// Charwise splices inline; linewise opens whole lines; blockwise drops a rectangle, one stored row per
/// buffer line at a fixed column. This is the typed SHAPE of F-029 (`:help quote_bar`, blockwise via
/// `CTRL-V`).
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub enum RegKind {
    /// A partial-line span (`yw`, `x`, charwise `d`).
    #[default]
    Charwise,
    /// Whole lines (`yy`, `dd`); content is normalized to a trailing newline.
    Linewise,
    /// A rectangle (`CTRL-V` yank/delete): the `\n`-joined per-row slices, pasted column-aligned. Rows are
    /// ragged (each is its line's own content within the block); paste pads short target lines with spaces.
    Blockwise,
}

/// A captured span of text and its paste geometry ([`RegKind`]). Linewise content is normalized to end
/// with a newline so paste geometry is uniform; blockwise content is the per-row slices joined by `\n`
/// (N rows → N-1 separators, no trailing newline).
#[derive(Clone, Default, PartialEq, Eq, Debug)]
pub struct Register {
    text: Vec<u8>,
    kind: RegKind,
}

impl Register {
    /// A charwise register (a partial-line span: `yw`, `x`, charwise `d`).
    #[must_use]
    pub fn charwise(text: Vec<u8>) -> Register {
        Register {
            text,
            kind: RegKind::Charwise,
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
            kind: RegKind::Linewise,
        }
    }

    /// A blockwise register (`CTRL-V` yank/delete): the per-row slices already joined by `\n`. Stored
    /// verbatim (ragged rows, no trailing newline); the paste path pads short target lines.
    #[must_use]
    pub fn blockwise(text: Vec<u8>) -> Register {
        Register {
            text,
            kind: RegKind::Blockwise,
        }
    }

    /// The stored bytes.
    #[must_use]
    pub fn text(&self) -> &[u8] {
        &self.text
    }

    /// The register's paste geometry.
    #[must_use]
    pub fn kind(&self) -> RegKind {
        self.kind
    }

    /// Whether the register holds whole lines (governs paste geometry).
    #[must_use]
    pub fn is_linewise(&self) -> bool {
        self.kind == RegKind::Linewise
    }

    /// Whether the register holds a rectangle (blockwise paste geometry).
    #[must_use]
    pub fn is_blockwise(&self) -> bool {
        self.kind == RegKind::Blockwise
    }

    /// Whether the register is empty (nothing has been yanked or deleted yet — paste is a no-op).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }
}

/// The register STORE: the unnamed slot, the 26 named slots `a`–`z` (D-026's additive expansion over the
/// single-slot model), and the yank register `"0`. The numbered delete-ring (`"1`–`"9`) is still deferred
/// (captured by the oracle but not yet modelled), so nothing here fakes it.
///
/// Vim linkage (`:help registers`): a yank/delete/change into `"x` ALSO mirrors into the unnamed register
/// (unnamed always reflects the LAST write); a plain, unregistered edit writes the unnamed slot only. An
/// UPPERCASE name (`"A`–`"Z`) APPENDS to the lowercase slot instead of replacing it, and the unnamed slot
/// then mirrors the full appended content. The yank register `"0` holds the text of the most recent YANK
/// (`:help quote0`) — but ONLY when the yank named no other register; a delete/change never touches it, so
/// `"0` survives intervening deletes. `"0` is read-only from the edit path (you paste from it, `"0p`).
#[derive(Clone, Debug)]
pub struct RegisterStore {
    unnamed: Register,
    /// Slots for `a`–`z`, indexed by `letter - 'a'`. Uppercase names append into the same 26 slots.
    named: [Register; 26],
    /// The yank register `"0`: the last unregistered yank, untouched by deletes/changes.
    yank0: Register,
}

impl Default for RegisterStore {
    fn default() -> RegisterStore {
        RegisterStore {
            unnamed: Register::default(),
            named: std::array::from_fn(|_| Register::default()),
            yank0: Register::default(),
        }
    }
}

impl RegisterStore {
    /// A fresh store: every slot empty.
    #[must_use]
    pub fn new() -> RegisterStore {
        RegisterStore::default()
    }

    /// The unnamed register (the last write, whatever slot it targeted).
    #[must_use]
    pub fn unnamed(&self) -> &Register {
        &self.unnamed
    }

    /// The slot index for an `a`–`z`/`A`–`Z` name, or `None` for any other char (an unsupported register).
    fn index(name: char) -> Option<usize> {
        name.is_ascii_alphabetic()
            .then(|| (name.to_ascii_lowercase() as u8 - b'a') as usize)
    }

    /// The yank register `"0` (the last unregistered yank).
    #[must_use]
    pub fn yank0(&self) -> &Register {
        &self.yank0
    }

    /// Read a register for a paste. `None` → unnamed; `'0'` → the yank register; a named letter
    /// (case-insensitive) → its slot; any unsupported name falls back to the unnamed register rather than
    /// inventing an empty one.
    #[must_use]
    pub fn get(&self, name: Option<char>) -> &Register {
        match name {
            Some('0') => &self.yank0,
            Some(c) => match Self::index(c) {
                Some(i) => &self.named[i],
                None => &self.unnamed,
            },
            None => &self.unnamed,
        }
    }

    /// Write a captured value on a delete/change (NOT a yank — see [`RegisterStore::yank`]). `None` writes
    /// the unnamed slot only. A named letter writes (lowercase) or appends (uppercase) into its slot, and
    /// mirrors the resulting content into the unnamed slot (Vim's "unnamed reflects the last write"). An
    /// unsupported name (including `'0'`, which is read-only from the edit path) degrades to unnamed-only.
    pub fn write(&mut self, name: Option<char>, reg: Register) {
        match name {
            None => self.unnamed = reg,
            Some(c) => match Self::index(c) {
                Some(i) => {
                    self.named[i] = if c.is_ascii_uppercase() {
                        append(&self.named[i], &reg)
                    } else {
                        reg
                    };
                    self.unnamed = self.named[i].clone();
                }
                None => self.unnamed = reg,
            },
        }
    }

    /// Write a captured value on a YANK. Same slot routing as [`RegisterStore::write`], plus: an
    /// unregistered yank (`name` is `None`) ALSO seeds the yank register `"0` (Vim `:help quote0`). A yank
    /// into a named register does NOT touch `"0`.
    pub fn yank(&mut self, name: Option<char>, reg: Register) {
        if name.is_none() {
            self.yank0 = reg.clone();
        }
        self.write(name, reg);
    }
}

/// Append `add` onto `existing`, matching Vim's `"A`-style accumulation. Appending into an empty slot just
/// takes `add`. Charwise + charwise concatenates directly (`"Ayiw` twice → "foobar"); once either side is
/// linewise the result is linewise, with a separating newline inserted only when a charwise head precedes a
/// linewise tail (a linewise head already carries its trailing newline).
fn append(existing: &Register, add: &Register) -> Register {
    if existing.is_empty() {
        return add.clone();
    }
    let mut bytes = existing.text().to_vec();
    if existing.is_linewise() {
        // Normalized linewise content already ends in '\n'; the tail joins straight on.
        bytes.extend_from_slice(add.text());
    } else if add.is_linewise() {
        bytes.push(b'\n');
        bytes.extend_from_slice(add.text());
    } else {
        bytes.extend_from_slice(add.text());
    }
    if existing.is_linewise() || add.is_linewise() {
        Register::linewise(bytes)
    } else {
        Register::charwise(bytes)
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

    #[test]
    fn store_named_write_mirrors_unnamed() {
        // A write into `"a` fills the named slot AND the unnamed slot (Vim's last-write mirror).
        let mut s = RegisterStore::new();
        s.write(Some('a'), Register::charwise(b"foo".to_vec()));
        assert_eq!(s.get(Some('a')).text(), b"foo");
        assert_eq!(s.unnamed().text(), b"foo");
        // A plain (unregistered) write touches unnamed only, leaving `"a` intact.
        s.write(None, Register::charwise(b"bar".to_vec()));
        assert_eq!(s.unnamed().text(), b"bar");
        assert_eq!(
            s.get(Some('a')).text(),
            b"foo",
            "named slot survives a plain write"
        );
    }

    #[test]
    fn store_uppercase_appends_charwise() {
        // `"Ayiw` twice concatenates directly (matches the nvim oracle: "foo"+"bar" -> "foobar").
        let mut s = RegisterStore::new();
        s.write(Some('A'), Register::charwise(b"foo".to_vec()));
        s.write(Some('A'), Register::charwise(b"bar".to_vec()));
        assert_eq!(s.get(Some('a')).text(), b"foobar");
        assert!(!s.get(Some('a')).is_linewise());
        assert_eq!(
            s.unnamed().text(),
            b"foobar",
            "unnamed mirrors the full appended content"
        );
    }

    #[test]
    fn store_uppercase_appends_linewise() {
        // `"Ayy` twice accumulates whole lines (oracle: "alpha\n"+"beta\n").
        let mut s = RegisterStore::new();
        s.write(Some('A'), Register::linewise(b"alpha".to_vec()));
        s.write(Some('A'), Register::linewise(b"beta".to_vec()));
        assert!(s.get(Some('a')).is_linewise());
        assert_eq!(s.get(Some('a')).text(), b"alpha\nbeta\n");
    }

    #[test]
    fn yank0_holds_last_unregistered_yank_and_survives_a_delete() {
        let mut s = RegisterStore::new();
        // An unregistered yank seeds "0 and mirrors unnamed.
        s.yank(None, Register::charwise(b"foo".to_vec()));
        assert_eq!(s.yank0().text(), b"foo");
        assert_eq!(s.get(Some('0')).text(), b"foo");
        assert_eq!(s.unnamed().text(), b"foo");
        // A later delete overwrites unnamed but NOT "0 (Vim quote0: "0 survives deletes).
        s.write(None, Register::charwise(b"bar".to_vec()));
        assert_eq!(s.unnamed().text(), b"bar");
        assert_eq!(s.yank0().text(), b"foo", "\"0 is untouched by a delete");
    }

    #[test]
    fn yank_into_a_named_register_leaves_yank0_empty() {
        let mut s = RegisterStore::new();
        // `"ayiw`: a yank naming another register must NOT set "0 (:help quote0).
        s.yank(Some('a'), Register::charwise(b"foo".to_vec()));
        assert_eq!(s.get(Some('a')).text(), b"foo");
        assert!(s.yank0().is_empty(), "a named yank does not touch \"0");
    }

    #[test]
    fn store_read_case_insensitive_and_fallback() {
        let mut s = RegisterStore::new();
        s.write(Some('a'), Register::charwise(b"z".to_vec()));
        assert_eq!(
            s.get(Some('A')).text(),
            b"z",
            "uppercase reads the same slot"
        );
        assert!(s.get(Some('q')).is_empty(), "an untouched slot is empty");
    }
}
