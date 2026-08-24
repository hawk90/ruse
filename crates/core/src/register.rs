//! The register store — each slot carries yanked/deleted text plus its *type* (charwise/linewise/blockwise),
//! which governs paste geometry. This implements the Vim surface of D-026: the unnamed slot, named slots
//! (`"a`–`"z`/`"A`–`"Z`), the yank register `"0`, the numbered delete-ring `"1`–`"9`, the small-delete
//! register `"-`, and the blackhole `"_`. The Emacs kill-ring (bounded ordered ring, coalescing, yank-pop)
//! is still **deferred** (see `docs/design/register-model.md`).

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
/// single-slot model), the yank register `"0`, the numbered delete-ring `"1`–`"9`, the small-delete
/// register `"-`, and the blackhole `"_`.
///
/// Vim linkage (`:help registers`): a yank/delete/change into `"x` ALSO mirrors into the unnamed register
/// (unnamed always reflects the LAST write); a plain, unregistered edit writes the unnamed slot only. An
/// UPPERCASE name (`"A`–`"Z`) APPENDS to the lowercase slot instead of replacing it, and the unnamed slot
/// then mirrors the full appended content. The yank register `"0` holds the text of the most recent YANK
/// (`:help quote0`) — but ONLY when the yank named no other register; a delete/change never touches it, so
/// `"0` survives intervening deletes. `"0` is read-only from the edit path (you paste from it, `"0p`).
///
/// Delete rings (`:help quote_number`, `:help quote-`): an UNNAMED delete/change of a whole line or more
/// (linewise, or any span crossing a `\n`) shifts the numbered ring — `"1`→`"2`…→`"9` (the old `"9` is
/// lost) — and lands in `"1`. An unnamed delete of LESS than one line goes to the small-delete register
/// `"-` instead, leaving the numbered ring untouched. A delete that NAMES a register touches neither ring
/// (Vim), and yanks never touch either. All are read-only from the edit path (`"1p`…`"9p`, `"-p`).
#[derive(Clone, Debug)]
pub struct RegisterStore {
    unnamed: Register,
    /// Slots for `a`–`z`, indexed by `letter - 'a'`. Uppercase names append into the same 26 slots.
    named: [Register; 26],
    /// The yank register `"0`: the last unregistered yank, untouched by deletes/changes.
    yank0: Register,
    /// The numbered delete-ring `"1`–`"9`, indexed by `digit - '1'` (`numbered[0]` is `"1`). A qualifying
    /// unnamed delete shifts every slot down one and writes `numbered[0]`.
    numbered: [Register; 9],
    /// The small-delete register `"-`: the last unnamed delete of less than one line.
    small_delete: Register,
    /// The blackhole register `"_`: always empty. A write/yank/delete NAMING it is discarded (nothing else,
    /// including the unnamed slot and the delete rings, is touched); a read yields nothing (Vim `:help quote_`).
    blackhole: Register,
    /// The system-clipboard mirror for `"+` and `"*` (`:help quoteplus`). This slot carries the paste
    /// geometry ([`RegKind`]) the way any register does, so a linewise `"+yy` still pastes as a whole line;
    /// the actual OS clipboard is an impure side effect the [`Workspace`](crate::Workspace) syncs this slot
    /// with through its injected [`Clipboard`](crate::clipboard::Clipboard). For v0 `+` and `*` are the SAME
    /// slot (correct on macOS/Windows; on X11 `*` is really the PRIMARY selection — an accepted v0 divergence).
    clipboard: Register,
}

impl Default for RegisterStore {
    fn default() -> RegisterStore {
        RegisterStore {
            unnamed: Register::default(),
            named: std::array::from_fn(|_| Register::default()),
            yank0: Register::default(),
            numbered: std::array::from_fn(|_| Register::default()),
            small_delete: Register::default(),
            blackhole: Register::default(),
            clipboard: Register::default(),
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

    /// Whether `name` selects the system clipboard (`"+` or `"*`). For v0 both map to the same slot (see
    /// [`RegisterStore::clipboard`]).
    #[must_use]
    pub fn is_clipboard(name: Option<char>) -> bool {
        matches!(name, Some('+') | Some('*'))
    }

    /// The system-clipboard mirror slot (`"+`/`"*`). The [`Workspace`](crate::Workspace) reads this after a
    /// clipboard yank/delete to push it to the OS, and refreshes it before a clipboard paste via
    /// [`RegisterStore::set_clipboard_from_external`].
    #[must_use]
    pub fn clipboard(&self) -> &Register {
        &self.clipboard
    }

    /// Replace the clipboard mirror slot from OS-clipboard `text` (untyped bytes), inferring paste geometry:
    /// a trailing newline reads as linewise, anything else as charwise (Vim's clipboard heuristic). When the
    /// incoming bytes are byte-identical to the slot's current content the slot is left UNTOUCHED, preserving
    /// the exact [`RegKind`] a just-completed in-session `"+y` recorded (so a linewise `"+yy` → `"+p` still
    /// opens a whole line even though the OS clipboard only round-tripped the bytes).
    pub fn set_clipboard_from_external(&mut self, text: Vec<u8>) {
        if self.clipboard.text() == text.as_slice() {
            return;
        }
        self.clipboard = if text.last() == Some(&b'\n') {
            Register::linewise(text)
        } else {
            Register::charwise(text)
        };
    }

    /// The yank register `"0` (the last unregistered yank).
    #[must_use]
    pub fn yank0(&self) -> &Register {
        &self.yank0
    }

    /// Read a register for a paste. `None` → unnamed; `'0'` → the yank register; `'1'`–`'9'` → the numbered
    /// delete-ring; `'-'` → the small-delete register; a named letter (case-insensitive) → its slot; any
    /// unsupported name falls back to the unnamed register rather than inventing an empty one.
    #[must_use]
    pub fn get(&self, name: Option<char>) -> &Register {
        match name {
            Some('0') => &self.yank0,
            Some(c @ '1'..='9') => &self.numbered[c as usize - '1' as usize],
            Some('-') => &self.small_delete,
            Some('_') => &self.blackhole,
            Some('+') | Some('*') => &self.clipboard,
            Some(c) => match Self::index(c) {
                Some(i) => &self.named[i],
                None => &self.unnamed,
            },
            None => &self.unnamed,
        }
    }

    /// The numbered delete-ring slot `"1`–`"9` (`n` in `1..=9`), or `None` for an out-of-range index.
    #[must_use]
    pub fn numbered(&self, n: usize) -> Option<&Register> {
        (1..=9).contains(&n).then(|| &self.numbered[n - 1])
    }

    /// The small-delete register `"-` (the last unnamed delete of less than one line).
    #[must_use]
    pub fn small_delete(&self) -> &Register {
        &self.small_delete
    }

    /// Write a captured value on a delete/change (NOT a yank — see [`RegisterStore::yank`]). `None` writes
    /// the unnamed slot only. A named letter writes (lowercase) or appends (uppercase) into its slot, and
    /// mirrors the resulting content into the unnamed slot (Vim's "unnamed reflects the last write"). An
    /// unsupported name (including `'0'`, which is read-only from the edit path) degrades to unnamed-only.
    pub fn write(&mut self, name: Option<char>, reg: Register) {
        match name {
            None => self.unnamed = reg,
            // The blackhole `"_` swallows the write — the unnamed slot and every other register are untouched.
            Some('_') => {}
            // `"+`/`"*` write the clipboard mirror; like a named write they also mirror the unnamed slot
            // (Vim: a yank/delete fills the unnamed register regardless of which register was named).
            Some('+') | Some('*') => {
                self.clipboard = reg;
                self.unnamed = self.clipboard.clone();
            }
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

    /// Route a DELETE/CHANGE capture: the same slot write as [`RegisterStore::write`], plus the Vim
    /// delete-ring bookkeeping. Only an UNNAMED delete touches a ring (a named delete affects only its slot
    /// and unnamed, per Vim): a whole-line-or-more span (linewise, or any span containing a `\n`) shifts the
    /// numbered ring `"1`→…→`"9` and lands in `"1`; a smaller span lands in the small-delete register `"-`.
    pub fn delete(&mut self, name: Option<char>, reg: Register) {
        if name.is_none() {
            if reg.is_linewise() || reg.text().contains(&b'\n') {
                // Shift "1→"2 … "8→"9 (dropping the old "9), then write the new delete into "1.
                self.numbered.rotate_right(1);
                self.numbered[0] = reg.clone();
            } else {
                self.small_delete = reg.clone();
            }
        }
        self.write(name, reg);
    }

    /// Store a MACRO into a named register (D-055): a lowercase name OVERWRITES its slot, an uppercase name
    /// APPENDS to it (`qA` extends macro `a`). Unlike [`RegisterStore::write`], this does NOT mirror into the
    /// unnamed slot — recording a macro must not clobber the paste register (Vim behaviour). A non-letter
    /// name is ignored (a macro must name `a`-`z`/`A`-`Z`).
    pub fn set_macro(&mut self, name: Option<char>, reg: Register) {
        if let Some(c) = name {
            if let Some(i) = Self::index(c) {
                self.named[i] = if c.is_ascii_uppercase() {
                    append(&self.named[i], &reg)
                } else {
                    reg
                };
            }
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

    /// Append a killed span onto the unnamed slot — Emacs kill-accumulation, where a kill immediately
    /// following another kill grows the current kill-ring entry rather than starting a new one. Uses the
    /// same `"A`-style [`append`] geometry as a named append. The kill-ring is the unnamed register (D-026);
    /// the caller decides WHEN to accumulate (the `last-command`-was-a-kill test lives in `commit`).
    pub fn kill_append(&mut self, reg: Register) {
        self.unnamed = append(&self.unnamed, &reg);
    }

    /// Prepend a killed span onto the unnamed slot — the BACKWARD-kill form of [`RegisterStore::kill_append`]
    /// (Emacs `kill-append` with `before=t`): a `backward-kill-word` right after another kill grows the
    /// current entry from the FRONT, since it takes text preceding the prior kill.
    pub fn kill_prepend(&mut self, reg: Register) {
        self.unnamed = append(&reg, &self.unnamed);
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

    #[test]
    fn set_macro_appends_uppercase_and_never_mirrors_unnamed() {
        let mut s = RegisterStore::new();
        // Lowercase overwrites, and (unlike write) does NOT touch the unnamed slot (D-055).
        s.set_macro(Some('a'), Register::charwise(b"iZ".to_vec()));
        assert_eq!(s.get(Some('a')).text(), b"iZ");
        assert!(
            s.unnamed().is_empty(),
            "recording a macro must not clobber the paste register"
        );
        // Uppercase appends onto the same slot (`qA`), still without mirroring.
        s.set_macro(Some('A'), Register::charwise(b"jj".to_vec()));
        assert_eq!(s.get(Some('a')).text(), b"iZjj", "uppercase name appends");
        assert!(s.unnamed().is_empty(), "still no unnamed mirror");
    }

    #[test]
    fn numbered_ring_shifts_on_whole_line_deletes() {
        let mut s = RegisterStore::new();
        // Three linewise deletes: each shifts the ring so "1 is the newest, "3 the oldest.
        s.delete(None, Register::linewise(b"one".to_vec()));
        s.delete(None, Register::linewise(b"two".to_vec()));
        s.delete(None, Register::linewise(b"three".to_vec()));
        assert_eq!(
            s.get(Some('1')).text(),
            b"three\n",
            "\"1 is the newest delete"
        );
        assert_eq!(s.get(Some('2')).text(), b"two\n");
        assert_eq!(s.get(Some('3')).text(), b"one\n", "\"3 is the oldest");
        assert_eq!(
            s.unnamed().text(),
            b"three\n",
            "unnamed mirrors the last delete"
        );
    }

    #[test]
    fn numbered_ring_drops_the_ninth_and_wraps_cleanly() {
        let mut s = RegisterStore::new();
        // Push 10 linewise deletes numbered 0..=9; the ring holds the last 9 (9 down to 1 in "1.."9).
        for i in 0..10 {
            s.delete(None, Register::linewise(vec![b'0' + i]));
        }
        assert_eq!(s.get(Some('1')).text(), b"9\n", "\"1 = most recent");
        assert_eq!(
            s.get(Some('9')).text(),
            b"1\n",
            "\"9 = 9th most recent (0 fell off)"
        );
        assert!(s.numbered(1).is_some() && s.numbered(9).is_some());
        assert!(s.numbered(0).is_none() && s.numbered(10).is_none());
    }

    #[test]
    fn small_delete_takes_sub_line_deletes_and_spares_the_ring() {
        let mut s = RegisterStore::new();
        s.delete(None, Register::linewise(b"line".to_vec())); // seeds "1
        s.delete(None, Register::charwise(b"word".to_vec())); // sub-line → "-, not the ring
        assert_eq!(s.small_delete().text(), b"word");
        assert_eq!(s.get(Some('-')).text(), b"word");
        assert_eq!(
            s.get(Some('1')).text(),
            b"line\n",
            "the ring is untouched by a small delete"
        );
        assert_eq!(
            s.unnamed().text(),
            b"word",
            "unnamed still mirrors the last delete"
        );
    }

    #[test]
    fn multiline_charwise_delete_uses_the_ring_not_small_delete() {
        let mut s = RegisterStore::new();
        // A charwise span that crosses a newline (e.g. `d}`) is a whole-line-or-more delete → the ring.
        s.delete(None, Register::charwise(b"a\nb".to_vec()));
        assert_eq!(s.get(Some('1')).text(), b"a\nb");
        assert!(
            s.small_delete().is_empty(),
            "a multi-line delete does not go to \"-"
        );
    }

    #[test]
    fn blackhole_discards_and_reads_empty() {
        let mut s = RegisterStore::new();
        s.yank(None, Register::charwise(b"keep".to_vec())); // seed unnamed + "0
                                                            // A delete/yank into "_ is swallowed: unnamed, "0, the ring, and "- are all untouched.
        s.delete(Some('_'), Register::linewise(b"gone".to_vec()));
        s.yank(Some('_'), Register::charwise(b"also gone".to_vec()));
        assert_eq!(s.unnamed().text(), b"keep", "\"_ never touches unnamed");
        assert_eq!(s.get(Some('0')).text(), b"keep", "\"_ never touches \"0");
        assert!(s.get(Some('1')).is_empty(), "\"_ never shifts the ring");
        assert!(s.small_delete().is_empty());
        assert!(
            s.get(Some('_')).is_empty(),
            "the blackhole always reads empty"
        );
    }

    #[test]
    fn clipboard_register_routes_plus_and_star_to_one_slot() {
        // `"+`/`"*` share ONE slot (v0), and — like any named write — also mirror the unnamed register.
        let mut s = RegisterStore::new();
        s.yank(Some('+'), Register::charwise(b"clip".to_vec()));
        assert_eq!(s.get(Some('+')).text(), b"clip");
        assert_eq!(
            s.get(Some('*')).text(),
            b"clip",
            "* reads the same slot as +"
        );
        assert_eq!(
            s.unnamed().text(),
            b"clip",
            "a clipboard yank mirrors unnamed"
        );
        assert!(s.yank0().is_empty(), "a yank naming + does not touch \"0");
    }

    #[test]
    fn clipboard_delete_spares_the_numbered_ring() {
        // `"+dd` is a NAMED delete: it fills the clipboard slot (and unnamed) but never the delete ring.
        let mut s = RegisterStore::new();
        s.delete(Some('+'), Register::linewise(b"gone".to_vec()));
        assert_eq!(s.get(Some('+')).text(), b"gone\n");
        assert!(
            s.get(Some('1')).is_empty(),
            "a clipboard delete does not shift the ring"
        );
        assert!(s.small_delete().is_empty());
    }

    #[test]
    fn clipboard_external_sync_infers_kind_but_preserves_in_session() {
        let mut s = RegisterStore::new();
        // An in-session linewise yank tags the slot linewise.
        s.yank(Some('+'), Register::linewise(b"line".to_vec()));
        assert!(s.get(Some('+')).is_linewise());
        // Re-pulling the byte-identical OS content preserves that linewise flag (Vim in-session geometry).
        s.set_clipboard_from_external(b"line\n".to_vec());
        assert!(
            s.get(Some('+')).is_linewise(),
            "unchanged bytes keep the kind"
        );
        // A DIFFERENT external value with no trailing newline reads back charwise.
        s.set_clipboard_from_external(b"external".to_vec());
        assert!(!s.get(Some('+')).is_linewise());
        assert_eq!(s.get(Some('+')).text(), b"external");
        // A trailing newline from outside reads back linewise.
        s.set_clipboard_from_external(b"whole\n".to_vec());
        assert!(s.get(Some('+')).is_linewise());
    }

    #[test]
    fn named_delete_leaves_both_rings_alone() {
        let mut s = RegisterStore::new();
        // `"add`: a named delete affects only its slot (and unnamed), never the numbered ring or "-.
        s.delete(Some('a'), Register::linewise(b"kept".to_vec()));
        assert_eq!(s.get(Some('a')).text(), b"kept\n");
        assert!(
            s.get(Some('1')).is_empty(),
            "a named delete does not shift the ring"
        );
        assert!(s.small_delete().is_empty());
    }
}
