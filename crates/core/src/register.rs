//! The register store — each slot carries yanked/deleted text plus its *type* (charwise/linewise/blockwise),
//! which governs paste geometry. This implements the Vim surface of D-026: the unnamed slot, named slots
//! (`"a`–`"z`/`"A`–`"Z`), the yank register `"0`, the numbered delete-ring `"1`–`"9`, the small-delete
//! register `"-`, and the blackhole `"_`. The Emacs kill-ring (bounded ordered ring, coalescing, yank-pop)
//! is still **deferred** (see `docs/design/register-model.md`).

/// The most bytes a register value may hold in a trace record (RFC-0020 R6): nvim's `INT_MAX`, past which
/// a put fails with E1240 ("Resulting text too long").
pub const MAX_REGISTER_BYTES: usize = i32::MAX as usize;

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
    /// A block's width in screen cells (Vim's `y_width + 1`): a paste pads short rows to it. 0 otherwise.
    width: usize,
    /// The register was written, maybe with nothing (`qaq` leaves `a` set but empty, #1091). A default
    /// register was never written: a put from it is E353.
    set: bool,
}

impl Register {
    /// A charwise register (a partial-line span: `yw`, `x`, charwise `d`).
    #[must_use]
    pub fn charwise(text: Vec<u8>) -> Register {
        Register {
            text,
            kind: RegKind::Charwise,
            width: 0,
            set: true,
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
            width: 0,
            set: true,
        }
    }

    /// A blockwise register (`CTRL-V` yank/delete): the per-row slices already joined by `\n`. Stored
    /// verbatim (ragged rows, no trailing newline); the paste path pads short target lines. Its width is
    /// the widest row in screen cells, a `<Tab>` counting to Vim's default tabstop of 8; the editor's own
    /// block ops know the real block and use [`Register::blockwise_width`].
    #[must_use]
    pub fn blockwise(text: Vec<u8>) -> Register {
        let width = text
            .split(|&c| c == b'\n')
            .map(|row| crate::editor::text_cells(row, 8))
            .max()
            .unwrap_or(0);
        Register::blockwise_width(text, width)
    }

    /// A blockwise register whose block is `width` columns wide, wider than its rows when their trailing
    /// white space was dropped (`zy`).
    #[must_use]
    pub fn blockwise_width(text: Vec<u8>, width: usize) -> Register {
        Register {
            text,
            kind: RegKind::Blockwise,
            width,
            set: true,
        }
    }

    /// A block's width in columns (0 for a charwise or linewise register).
    #[must_use]
    pub fn block_width(&self) -> usize {
        self.width
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

    /// Whether the register was ever written, even with nothing (`qaq`).
    #[must_use]
    pub fn is_set(&self) -> bool {
        self.set
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
/// `"0` survives intervening deletes. A yank or delete that names `"0` writes it (`"0yy`).
///
/// Delete rings (`:help quote_number`, `:help quote-`, nvim `op_delete`): a delete/change of a whole line
/// or more (linewise, or any span crossing a `\n`), or one over a motion that always uses `"1` (`%`, `(`,
/// `)`, `{`, `}`, `` ` ``, `/`, `?`, `n`, `N`, `*`, `#`), shifts the numbered ring — `"1`→`"2`…→`"9` (the old
/// `"9` is lost) — and lands in `"1`, whatever register was named. A delete of LESS than one line with no
/// register named also goes to the small-delete register `"-`. Yanks never touch either ring. `"0`, `"1`–`"9`
/// and `"-` can also be written by name (`"0yy`, `"-dw`); a delete names its register first, then shifts.
#[derive(Clone, Debug)]
pub struct RegisterStore {
    unnamed: Register,
    /// The letter slot the unnamed register last took its content from (`"ayy`): nvim's unnamed register
    /// points at that register, so re-recording it (`qaq`) changes what `p` puts (#1091).
    unnamed_from: Option<usize>,
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
    /// The expression register `"=` (`:help quote=`): unlike every other slot its contents are not yanked or
    /// deleted text but the (already-evaluated) result of an expression. The frontend collects the
    /// expression string via a prompt, the editor evaluates it (`crate::expr`) and stores the formatted
    /// result here with [`RegisterStore::set_expr`]; a following paste/insert reads it via `get(Some('='))`.
    /// Linewise when the result ends in a newline (nvim `"="x\n"`), else charwise. Empty when evaluation
    /// failed (Vim's degrade).
    expr: Register,
    /// The four READ-ONLY special registers (`:help quote_/`, `quote_:`, `quote_.`, `quote%`): `"/` = the
    /// last search pattern, `":` = the last executed Ex command-line (no leading `:`), `".` = the last
    /// inserted text, `"%` = the current file name. Their DATA lives frontend-side (the search ring, the ex
    /// plumbing, the last-insert record, the focused buffer's path), so the [`Workspace`](crate::Workspace)
    /// syncs it into these slots via [`RegisterStore::set_special`] before dispatching a register-reading
    /// command; `get` then returns them. Always charwise. An operator naming one fails in the editor (#1022).
    special: SpecialRegisters,
}

/// The four read-only special-register slots (`"/ ": ". "%`). Each carries an always-charwise [`Register`]
/// resolved from frontend state; see [`RegisterStore::special`].
#[derive(Clone, Debug, Default)]
struct SpecialRegisters {
    search: Register,
    last_ex: Register,
    inserted: Register,
    file: Register,
    /// `"#` — the alternate file name (`:help quote#`), synced by [`RegisterStore::set_alternate`].
    alternate: Register,
}

impl Default for RegisterStore {
    fn default() -> RegisterStore {
        RegisterStore {
            unnamed: Register::default(),
            unnamed_from: None,
            named: std::array::from_fn(|_| Register::default()),
            yank0: Register::default(),
            numbered: std::array::from_fn(|_| Register::default()),
            small_delete: Register::default(),
            blackhole: Register::default(),
            clipboard: Register::default(),
            expr: Register::default(),
            special: SpecialRegisters::default(),
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

    /// Whether a yank/delete may name `name` (nvim `valid_yank_reg(_, true)`): the unnamed register, a
    /// letter, a digit, `"-`, `"_`, `"#`, and the clipboard. The read-only `"/ ": ". "% "=`, the drop
    /// register `"~` and unknown names cannot be written: an operator naming one fails (#1022).
    #[must_use]
    pub fn is_writable(name: Option<char>) -> bool {
        name.is_none_or(|c| {
            c.is_ascii_alphanumeric() || matches!(c, '"' | '-' | '_' | '#' | '+' | '*')
        })
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

    /// Set exactly one slot to `reg`, with none of the mirroring, appending or ring shifting a yank, delete
    /// or macro store does: the trace's `ex_register` record (RFC-0020 R6), which carries a value the
    /// session wrote. `None`/`"` is the unnamed slot, then `"0`–`"9`, `"a`–`"z`, `"-`, `"+`/`"*`, and the
    /// read-only `"/ ": ". "% "#`. Returns `false` (nothing changed) for any other name.
    pub fn set_slot(&mut self, name: Option<char>, reg: Register) -> bool {
        let slot = match name {
            None | Some('"') => &mut self.unnamed,
            Some('0') => &mut self.yank0,
            Some(c @ '1'..='9') => &mut self.numbered[c as usize - '1' as usize],
            Some(c @ 'a'..='z') => &mut self.named[c as usize - 'a' as usize],
            Some('-') => &mut self.small_delete,
            Some('+' | '*') => &mut self.clipboard,
            Some('/') => &mut self.special.search,
            Some(':') => &mut self.special.last_ex,
            Some('.') => &mut self.special.inserted,
            Some('%') => &mut self.special.file,
            Some('#') => &mut self.special.alternate,
            Some(_) => return false,
        };
        *slot = reg;
        true
    }

    /// Whether [`RegisterStore::set_slot`] accepts `name`.
    #[must_use]
    pub fn is_slot(name: Option<char>) -> bool {
        name.is_none_or(|c| {
            c.is_ascii_digit()
                || c.is_ascii_lowercase()
                || matches!(c, '"' | '-' | '+' | '*' | '/' | ':' | '.' | '%' | '#')
        })
    }

    /// The registers a trace starts by writing (RFC-0020 R6): the unnamed register, `"0`–`"9`, `"a`–`"z`
    /// and `"-`, each that holds a value, in `:registers` order. The clipboard is not among them; it is
    /// recorded when it is read.
    #[must_use]
    pub fn filled(&self) -> Vec<(Option<char>, Register)> {
        std::iter::once((None, &self.unnamed))
            .chain(std::iter::once((Some('0'), &self.yank0)))
            .chain(('1'..='9').zip(&self.numbered).map(|(c, r)| (Some(c), r)))
            .chain(std::iter::once((Some('-'), &self.small_delete)))
            .chain(('a'..='z').zip(&self.named).map(|(c, r)| (Some(c), r)))
            .filter(|(_, r)| r.is_set())
            .map(|(n, r)| (n, r.clone()))
            .collect()
    }

    /// The yank register `"0` (the last unregistered yank).
    #[must_use]
    pub fn yank0(&self) -> &Register {
        &self.yank0
    }

    /// The expression register `"=` (`:help quote=`) — the last evaluated expression result.
    #[must_use]
    pub fn expr(&self) -> &Register {
        &self.expr
    }

    /// Store an evaluated expression result into the `"=` register: linewise when it ends in a newline, as
    /// nvim puts `"="x\n"` on a line of its own (#1022), else charwise. `result` is the already-formatted
    /// output of [`crate::expr::eval`] — an EMPTY string when evaluation failed, so a following paste/insert
    /// does nothing (Vim's degrade). This never touches any other slot.
    pub fn set_expr(&mut self, result: String) {
        let bytes = result.into_bytes();
        self.expr = if bytes.last() == Some(&b'\n') {
            Register::linewise(bytes)
        } else {
            Register::charwise(bytes)
        };
    }

    /// Sync the four read-only special registers (`:help quote_/`, `quote_:`, `quote_.`, `quote%`) from the
    /// frontend's current state, each as an always-charwise slot. `None` clears a slot (e.g. no search yet,
    /// or an unnamed buffer for `"%`). The [`Workspace`](crate::Workspace) calls this before dispatching a
    /// register-reading command so a following `"/p` / `C-r :` / `".p` / `"%p` resolves to the live value.
    ///
    /// - `search`  → `"/` — the last search pattern (`/`, `?`, `*`, `#`, `n`), no delimiters.
    /// - `last_ex` → `":` — the last executed Ex command-line, WITHOUT the leading `:`.
    /// - `inserted`→ `".` — the text of the most recent insert session.
    /// - `file`    → `"%` — the current file name as displayed (relative path); empty when unnamed.
    pub fn set_special(
        &mut self,
        search: Option<String>,
        last_ex: Option<String>,
        inserted: Option<String>,
        file: Option<String>,
    ) {
        let charwise = |s: Option<String>| Register::charwise(s.unwrap_or_default().into_bytes());
        self.special.search = charwise(search);
        self.special.last_ex = charwise(last_ex);
        self.special.inserted = charwise(inserted);
        self.special.file = charwise(file);
    }

    /// Sync `"#`, the alternate file name (`:help quote#`); `None` when there is no alternate file, so a
    /// `"#p` fails with E23.
    pub fn set_alternate(&mut self, name: Option<String>) {
        self.special.alternate = Register::charwise(name.unwrap_or_default().into_bytes());
    }

    /// Read a register for a paste. `None` → unnamed; `'0'` → the yank register; `'1'`–`'9'` → the numbered
    /// delete-ring; `'-'` → the small-delete register; `'/'`/`':'`/`'.'`/`'%'` → the read-only special
    /// registers (last search / last Ex line / last insert / file name, synced by [`set_special`]); `'#'` →
    /// the alternate file name; a named letter (case-insensitive) → its slot. The drop register `"~` and any
    /// unknown name read as an empty, never-written register (#1022), not as the unnamed one.
    ///
    /// [`set_special`]: RegisterStore::set_special
    #[must_use]
    pub fn get(&self, name: Option<char>) -> &Register {
        match name {
            Some('0') => &self.yank0,
            Some(c @ '1'..='9') => &self.numbered[c as usize - '1' as usize],
            Some('-') => &self.small_delete,
            Some('_') => &self.blackhole,
            Some('+') | Some('*') => &self.clipboard,
            Some('=') => &self.expr,
            // The read-only special registers (`:help quote_/`), synced from the frontend by `set_special`.
            Some('/') => &self.special.search,
            Some(':') => &self.special.last_ex,
            Some('.') => &self.special.inserted,
            Some('%') => &self.special.file,
            Some('#') => &self.special.alternate,
            Some('"') | None => &self.unnamed,
            Some(c) => match Self::index(c) {
                Some(i) => &self.named[i],
                None => &self.blackhole,
            },
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

    /// Write a captured value into the named slot and mirror it into the unnamed one (Vim's "unnamed
    /// reflects the last write"); no ring bookkeeping (see [`RegisterStore::delete`]). `None` writes the
    /// unnamed slot only. A letter writes (lowercase) or appends (uppercase) into its slot; `"0`, `"1`–`"9`
    /// and `"-` take the value as is, and `"#` writes `"0`, as nvim does (#749). A name that cannot be
    /// written ([`RegisterStore::is_writable`]) leaves every slot alone.
    pub fn write(&mut self, name: Option<char>, reg: Register) {
        if !Self::is_writable(name) || name == Some('_') {
            return;
        }
        self.unnamed_from = name.and_then(Self::index);
        match name {
            None | Some('"') => self.unnamed = reg,
            Some('0' | '#') => {
                self.yank0 = reg;
                self.unnamed = self.yank0.clone();
            }
            Some(c @ '1'..='9') => {
                let i = c as usize - '1' as usize;
                self.numbered[i] = reg;
                self.unnamed = self.numbered[i].clone();
            }
            Some('-') => {
                self.small_delete = reg;
                self.unnamed = self.small_delete.clone();
            }
            // `"+`/`"*` write the clipboard mirror; like a named write they also mirror the unnamed slot
            // (Vim: a yank/delete fills the unnamed register regardless of which register was named).
            Some('+') | Some('*') => {
                self.clipboard = reg;
                self.unnamed = self.clipboard.clone();
            }
            Some(c) => {
                if let Some(i) = Self::index(c) {
                    self.named[i] = if c.is_ascii_uppercase() {
                        append(&self.named[i], &reg)
                    } else {
                        reg
                    };
                    self.unnamed = self.named[i].clone();
                }
            }
        }
    }

    /// Route a DELETE/CHANGE capture: the same slot write as [`RegisterStore::write`], plus the Vim
    /// delete-ring bookkeeping (nvim `op_delete`). See [`RegisterStore::delete_ring`].
    pub fn delete(&mut self, name: Option<char>, reg: Register) {
        self.delete_ring(name, reg, false);
    }

    /// [`RegisterStore::delete`] with `ring_one` set for the motions that always use `"1` (`d%`, `d}`,
    /// `d/pat`, … — nvim's `use_reg_one`). In nvim's order: the named register takes the text first; then
    /// a span of a whole line or more, or a `ring_one` span, shifts the numbered ring `"1`→…→`"9` into `"1`
    /// whatever register was named (`"add` and `"+dd` too, #749); then a span within one line with no
    /// register named also lands in `"-`. The unnamed register follows `"1` after a shift, except after an
    /// append (`"Add`), where it keeps the whole appended register. The black hole and the registers a
    /// delete cannot name leave everything alone.
    pub fn delete_ring(&mut self, name: Option<char>, reg: Register, ring_one: bool) {
        if !Self::is_writable(name) || name == Some('_') {
            return;
        }
        let multiline = reg.is_linewise() || reg.text().contains(&b'\n');
        let small = name.is_none() && !multiline;
        if name.is_some() {
            self.write(name, reg.clone());
        }
        if multiline || ring_one {
            // Shift "1→"2 … "8→"9 (dropping the old "9), then write the new delete into "1.
            self.numbered.rotate_right(1);
            self.numbered[0] = reg.clone();
            if !name.is_some_and(|c| c.is_ascii_uppercase()) {
                self.unnamed = self.numbered[0].clone();
                self.unnamed_from = None;
            }
        }
        if small {
            self.small_delete = reg.clone();
        }
        if name.is_none() {
            self.unnamed = reg;
            self.unnamed_from = None;
        }
    }

    /// Store a MACRO into a named register (D-055): a lowercase name OVERWRITES its slot, an uppercase name
    /// APPENDS to it (`qA` extends macro `a`). Unlike [`RegisterStore::write`], this does NOT mirror into the
    /// unnamed slot — recording a macro must not clobber the paste register (Vim behaviour). A digit names a
    /// numbered register and `"` the unnamed one (`q{0-9a-zA-Z"}`); any other name is ignored.
    pub fn set_macro(&mut self, name: Option<char>, reg: Register) {
        match name {
            // `q"` records into `"0`, which the unnamed register then names (nvim v0.12.6: `q"lqP` puts `l`).
            Some('"') => {
                self.yank0 = reg.clone();
                self.unnamed = reg;
                self.unnamed_from = None;
                return;
            }
            Some('0') => {
                self.yank0 = reg;
                return;
            }
            Some(c @ '1'..='9') => {
                self.numbered[c as usize - '1' as usize] = reg;
                return;
            }
            _ => {}
        }
        if let Some(c) = name {
            if let Some(i) = Self::index(c) {
                self.named[i] = if c.is_ascii_uppercase() {
                    append(&self.named[i], &reg)
                } else {
                    reg
                };
                if self.unnamed_from == Some(i) {
                    self.unnamed = self.named[i].clone();
                }
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
    fn clipboard_delete_feeds_the_numbered_ring() {
        // `"+dd` fills the clipboard slot and, being a whole line, `"1` too (nvim v0.12.6, #749); a
        // charwise `"+dw` is named, so it spares `"-`.
        let mut s = RegisterStore::new();
        s.delete(Some('+'), Register::linewise(b"gone".to_vec()));
        assert_eq!(s.get(Some('+')).text(), b"gone\n");
        assert_eq!(s.get(Some('1')).text(), b"gone\n");
        s.delete(Some('+'), Register::charwise(b"w".to_vec()));
        assert!(s.small_delete().is_empty());
        assert_eq!(s.get(Some('1')).text(), b"gone\n");
    }

    #[test]
    fn explicit_ring_names_are_written_and_unknown_names_read_empty() {
        // `"0yy`, `"1dw`, `"-dw` write those slots (#749); `"#yy` writes `"0` as nvim does.
        let mut s = RegisterStore::new();
        s.yank(Some('0'), Register::charwise(b"z".to_vec()));
        assert_eq!(s.yank0().text(), b"z");
        s.delete(Some('1'), Register::charwise(b"one".to_vec()));
        assert_eq!(s.get(Some('1')).text(), b"one");
        s.delete(Some('-'), Register::charwise(b"dash".to_vec()));
        assert_eq!(s.small_delete().text(), b"dash");
        assert_eq!(s.unnamed().text(), b"dash");
        s.yank(Some('#'), Register::charwise(b"h".to_vec()));
        assert_eq!(s.yank0().text(), b"h");
        // The ring-one motions shift "1 even within a line, and an unnamed one fills "- too.
        s.delete_ring(None, Register::charwise(b"(x)".to_vec()), true);
        assert_eq!(s.get(Some('1')).text(), b"(x)");
        assert_eq!(s.get(Some('2')).text(), b"one");
        assert_eq!(s.small_delete().text(), b"(x)");
        // `"~`, `"!` and `"#` with no alternate file read as empty, never as the unnamed register (#1022).
        for c in ['~', '!', '?', '#'] {
            assert!(s.get(Some(c)).is_empty() && !s.get(Some(c)).is_set(), "{c}");
        }
        s.set_alternate(Some("other.txt".into()));
        assert_eq!(s.get(Some('#')).text(), b"other.txt");
        // A write naming a read-only or unknown register touches nothing.
        let before = s.unnamed().clone();
        for c in ['/', ':', '.', '%', '=', '~', '!'] {
            assert!(!RegisterStore::is_writable(Some(c)), "{c}");
            s.delete(Some(c), Register::linewise(b"no".to_vec()));
        }
        assert_eq!(s.unnamed(), &before);
        assert_eq!(s.get(Some('1')).text(), b"(x)");
    }

    #[test]
    fn named_delete_then_shift_points_unnamed_at_reg1_unless_appending() {
        // `"addqaq` still puts the deleted line: the unnamed register names "1, not "a (nvim).
        let mut s = RegisterStore::new();
        s.delete(Some('a'), Register::linewise(b"a".to_vec()));
        s.set_macro(Some('a'), Register::charwise(Vec::new()));
        assert_eq!(s.unnamed().text(), b"a\n");
        // `"ayy` then `"Add`: the unnamed register holds the whole appended "a.
        let mut s = RegisterStore::new();
        s.yank(Some('a'), Register::linewise(b"a".to_vec()));
        s.delete(Some('A'), Register::linewise(b"b".to_vec()));
        assert_eq!(s.unnamed().text(), b"a\nb\n");
        assert_eq!(s.get(Some('1')).text(), b"b\n");
    }

    #[test]
    fn expr_result_ending_in_newline_is_linewise() {
        let mut s = RegisterStore::new();
        s.set_expr("x\n".into());
        assert!(s.expr().is_linewise());
        s.set_expr("x".into());
        assert!(!s.expr().is_linewise());
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
    fn special_registers_resolve_from_set_special() {
        // `"/ ": ". "%` read the frontend-synced values, always charwise (`:help quote_/`).
        let mut s = RegisterStore::new();
        s.set_special(
            Some("pat".into()),
            Some("wq".into()),
            Some("typed".into()),
            Some("src/main.rs".into()),
        );
        assert_eq!(s.get(Some('/')).text(), b"pat");
        assert_eq!(s.get(Some(':')).text(), b"wq");
        assert_eq!(s.get(Some('.')).text(), b"typed");
        assert_eq!(s.get(Some('%')).text(), b"src/main.rs");
        for c in ['/', ':', '.', '%'] {
            assert!(!s.get(Some(c)).is_linewise(), "special regs are charwise");
        }
    }

    #[test]
    fn special_registers_default_empty_and_none_clears() {
        let mut s = RegisterStore::new();
        // Untouched: every special slot is empty (an unnamed buffer / no search yet).
        for c in ['/', ':', '.', '%'] {
            assert!(s.get(Some(c)).is_empty());
        }
        // A `None` field clears its slot (e.g. focusing an unnamed buffer empties `"%`).
        s.set_special(Some("x".into()), None, None, None);
        assert_eq!(s.get(Some('/')).text(), b"x");
        assert!(s.get(Some('%')).is_empty(), "None clears `\"%`");
    }

    #[test]
    fn special_registers_are_read_only_from_the_edit_path() {
        let mut s = RegisterStore::new();
        s.set_special(Some("pat".into()), None, None, None);
        s.yank(None, Register::charwise(b"keep".to_vec())); // seed unnamed + "0
                                                            // A yank/delete NAMING a special register is swallowed: the slot, unnamed, "0 and the
                                                            // rings are all untouched (`"/ ": ". "%` are read-only, `:help quote_/`).
        s.yank(Some('/'), Register::charwise(b"clobber".to_vec()));
        s.delete(Some('%'), Register::linewise(b"line".to_vec()));
        assert_eq!(
            s.get(Some('/')).text(),
            b"pat",
            "\"/ unchanged by a yank into it"
        );
        assert!(
            s.get(Some('%')).is_empty(),
            "\"% unchanged by a delete into it"
        );
        assert_eq!(
            s.unnamed().text(),
            b"keep",
            "a special-reg write never touches unnamed"
        );
        assert_eq!(s.get(Some('0')).text(), b"keep", "nor \"0");
        assert!(s.get(Some('1')).is_empty(), "nor the numbered ring");
        assert!(s.small_delete().is_empty(), "nor the small-delete register");
        for name in ['/', ':', '.', '%'] {
            assert!(
                !RegisterStore::is_writable(Some(name)),
                "\"{name} is read-only"
            );
        }
        assert!(RegisterStore::is_writable(Some('a')));
    }

    #[test]
    fn named_delete_shifts_the_ring_but_spares_small_delete() {
        let mut s = RegisterStore::new();
        // `"add` also lands in "1 (nvim `op_delete`, #1091); a named small delete never fills "-.
        s.delete(None, Register::linewise(b"old".to_vec()));
        s.delete(Some('a'), Register::linewise(b"kept".to_vec()));
        assert_eq!(s.get(Some('a')).text(), b"kept\n");
        assert_eq!(s.get(Some('1')).text(), b"kept\n");
        assert_eq!(s.get(Some('2')).text(), b"old\n");
        s.delete(Some('b'), Register::charwise(b"w".to_vec()));
        assert!(s.small_delete().is_empty());
        assert_eq!(
            s.get(Some('1')).text(),
            b"kept\n",
            "a small named delete spares the ring"
        );
    }

    #[test]
    fn an_empty_recording_empties_the_register_the_unnamed_one_names() {
        let mut s = RegisterStore::new();
        // `"ayy` then `qaq`: the unnamed register still names `a`, now empty but set (#1091).
        s.yank(Some('a'), Register::linewise(b"abc".to_vec()));
        s.set_macro(Some('a'), Register::charwise(Vec::new()));
        assert!(s.get(Some('a')).is_empty() && s.get(Some('a')).is_set());
        assert!(s.unnamed().is_empty() && s.unnamed().is_set());
        // After an unnamed yank the unnamed register no longer follows `a`.
        s.yank(None, Register::charwise(b"x".to_vec()));
        s.set_macro(Some('a'), Register::charwise(b"q".to_vec()));
        assert_eq!(s.unnamed().text(), b"x");
        assert!(!Register::default().is_set());
    }
}
