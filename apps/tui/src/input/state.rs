//! The layers' owned transient state (KL-OBL-4, KL-OBL-5): the Normal-family axes ([`NormalState`],
//! [`Awaiting`]), the Insert layer's prefixes ([`InsertState`] and its collectors) and the activation
//! stack's return address ([`Suspended`]). Moved out of `input/mod.rs` as type definitions (#1493).

use ruse_core::ForcedWise;

use super::{mul_raw, Ns, OpPending};

/// The one-shot key-expectation axis — what the *next* key must supply. Held separately from the operator
/// and count axes (per input-engine.md, these are **orthogonal** and must not be crammed into one enum).
/// Exactly one variant holds between keystrokes, so illegal combinations (awaiting a find-target *and* a
/// text-object char at once) are unrepresentable — the class of hierarchy bug that ad-hoc booleans invite.
#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub(super) enum Awaiting {
    /// A fresh key: a count digit, operator, motion, action, or a pending-initiator (`f`/`F`/`t`/`T`).
    #[default]
    Nothing,
    /// After `f`/`F`/`t`/`T`: the next key is the search target char.
    FindTarget { forward: bool, till: bool },
    /// After `i`/`a`: the next key is the text-object selector (`w`, `(`, `"`, …). Armed only with an
    /// operator present (`diw`) OR in a selection mode (`viw`) — never bare in Normal (invariant, tested).
    TextObjectChar { inner: bool },
    /// After `g`: a second `g` completes `gg` (jump to the first line / `{count}gg`).
    GSecond,
    /// After `r`: the next key is the replacement char.
    ReplaceChar,
    /// After `gr`: the next key is the CLASSIC-Vim virtual-replace char (`{count}gr{char}`, the one-shot form
    /// of `gR`). The live count carries through so `3gr{char}` replaces three chars.
    VirtualReplaceChar,
    /// After `` ` `` (backtick): the next key names a mark to jump to — `.` (last change) or a named mark
    /// `a`–`z`. Any other key aborts. `keep_jumps` is the `` g` `` form: the jump leaves the jumplist alone.
    MarkJump { keep_jumps: bool },
    /// After `m`: the next key names the mark (`a`–`z`) to SET at the cursor. Any other key aborts.
    SetMarkChar,
    /// After `'` (apostrophe): the next key names a mark to jump to LINEWISE (first non-blank of its line) —
    /// `.` (last change) or `a`–`z`. Any other key aborts. `keep_jumps` is the `g'` form.
    MarkJumpLine { keep_jumps: bool },
    /// After `"`: the next key is the register NAME (`a`–`z`, or `A`–`Z` to append). It arms a one-shot
    /// pending register that the FOLLOWING yank/delete/change/paste targets — emitted as a
    /// [`Command::SetRegister`] the core applies before that command. `"` itself does not reset the count.
    RegisterSelect,
    /// After `]` or `[`: the next key selects a bracket command. Only the indent-adjusting pastes are wired
    /// so far — `]p`/`]P`/`[p`/`[P` ([`Command::PasteIndent`]). `open` records which bracket started it (`]`
    /// pastes below like `p`, `[` above like `P`, before the second key's own `p`/`P` refines it).
    BracketPrefix { open_bracket: bool },
    /// After `Z`: `ZZ` (`:x`, write when modified and close the window) or `ZQ` (`:q!`). Any other key aborts.
    ZSecond,
    /// `CTRL-\` waits for `CTRL-N` / `CTRL-G` (go to Normal mode).
    CtrlBackslash,
}

/// The Normal-grammar layer's OWNED state (KL-OBL-4): the three orthogonal transient axes of the
/// Normal / Visual / Select / Operator-pending family, which share one grammar. It is a field of the
/// engine, but it BELONGS to that layer family — it is dropped (`NormalState::default()`) the instant
/// the active namespace leaves the family (into Insert / Replace / Cmdline / Terminal), so the count
/// or armed operator can never survive into another layer. The engine no longer reaches in to reset
/// individual axes on a mode change; the layer's state dies with the layer.
#[derive(Default)]
pub(super) struct NormalState {
    /// Count axis: the accumulating numeric prefix for the next motion/operator.
    pub(super) count: u32,
    /// The RAW count typed before a `"x` register prefix (`0` = none). A register prefix ends a count
    /// segment without consuming it: nvim multiplies the segments, `2"a3x` deletes 6 (#796).
    pub(super) reg_count: u32,
    /// A `"x` register prefix has been emitted for the command being built. Not a count, but it makes the
    /// state non-pristine (the next key still belongs to the same command).
    pub(super) register: bool,
    /// Operator axis: an armed operator awaiting its motion (`None` = none).
    pub(super) op: Option<OpPending>,
    /// Key-expectation axis: what the next key must supply (the top-priority resolution tier).
    pub(super) awaiting: Awaiting,
    /// A forced motion wise armed by `v`/`V` after an operator (Vim `o_v`/`o_V`): the NEXT motion
    /// resolves into a [`Command::OpForced`] instead of a plain operator command. `None` unless
    /// mid-`dv…`/`dV…`.
    pub(super) forced_wise: Option<ForcedWise>,
}

impl NormalState {
    /// A PRISTINE Normal base: no count, no armed operator, no key-expectation, no forced wise. The Native
    /// leader tier (F-013 NAT-2) only arms from here, so a Space mid-construct (`d<Space>`, `2<Space>`)
    /// stays the Vim right-motion — the text grammar is untouched (NAT-1).
    pub(super) fn is_clean(&self) -> bool {
        self.typed() == 0 && !self.register && !self.armed() && self.awaiting == Awaiting::Nothing
    }

    /// An operator is armed (or its forced wise, `dv`): the state [`InputEngine::pending`] reports as
    /// [`Pending::Operator`], and the one in which [`vim::continues_op`] decides whether a key continues or
    /// aborts the construct (#790).
    pub(super) fn armed(&self) -> bool {
        self.op.is_some() || self.forced_wise.is_some()
    }

    /// The RAW effective count of the command being built: the product of every count segment typed so far
    /// — before a register prefix, before the operator, and after it (`2"a3d2l` = 12) — or `0` when no
    /// count was typed at all. This is the ONE count model every consumer reads (#789, #791, #796, #799).
    pub(super) fn typed(&self) -> u32 {
        mul_raw(
            mul_raw(self.reg_count, self.op.map_or(0, |p| p.count)),
            self.count,
        )
    }
}

/// A SUSPENDED layer awaiting return (KL-OBL-5): while a one-shot command is borrowed to run in
/// another namespace, this records the ADDRESS to resume — *whence* control came. `i_CTRL-O` suspends
/// Insert to run one Normal command (`resume: Insert`); `t_CTRL-\ CTRL-O` (deferred, no terminal
/// buffers yet) is the SAME construct with `resume: Terminal`. A flat boolean edge cannot record
/// whence; a stack of these can, and nests for free.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) struct Suspended {
    /// The namespace to resume once the borrowed one-shot command completes.
    pub(super) resume: Ns,
}

/// The Insert layer's OWNED transient state (KL-OBL-4): the one-key `CTRL-G` prefix local to Insert.
/// Dropped (`InsertState::default()`) when the active namespace is not Insert — the layer's state dies
/// with the layer. (The `i_CTRL-O` one-shot is no longer a bool here; its RETURN ADDRESS lives on the
/// engine's activation stack — KL-OBL-5 — because a return spans two layers, not one.)
#[derive(Default)]
pub(super) struct InsertState {
    /// Insert-mode `CTRL-G` prefix: the next key is expected to be `u` (undo-break,
    /// [`Command::BreakUndo`]). A one-key expectation local to Insert; any other second key aborts it.
    pub(super) ctrl_g: bool,
    /// Insert-mode `CTRL-R` prefix: the next key is the register NAME whose contents to insert at the caret
    /// ([`Command::InsertRegister`]). A one-key expectation local to Insert; a non-register key aborts it.
    pub(super) ctrl_r: bool,
    /// Insert-mode `CTRL-K` digraph prefix (`i_CTRL-K`): a TWO-key expectation. `None` = inactive;
    /// `Some(DigraphPending::First)` = armed, awaiting the first of the two code chars;
    /// `Some(DigraphPending::Second(c1))` = first char collected, awaiting the second. Local to Insert.
    pub(super) digraph: Option<DigraphPending>,
    /// Insert-mode `CTRL-V` literal / numeric-code entry (`i_CTRL-V`): `None` = inactive;
    /// `Some(LiteralEntry::AwaitFirst)` = `CTRL-V` seen, the next key picks a numeric form or is inserted
    /// literally; `Some(LiteralEntry::Collecting{..})` = mid numeric collection. Local to Insert.
    pub(super) literal: Option<LiteralEntry>,
    /// Insert-mode keyword completion (`i_CTRL-N` / `i_CTRL-P`): the in-flight cycle across successive
    /// `CTRL-N`/`CTRL-P` keys. `None` = no active completion. Cleared when ANY other key is fed in Insert
    /// (that key ACCEPTS the current candidate, Vim's behavior) and, like the rest of `InsertState`, when
    /// the Insert layer dies. Local to Insert.
    pub(super) completion: Option<Completion>,
    /// Insert-mode `CTRL-X` completion submode (`i_CTRL-X`): the next key selects a completion SOURCE. Only
    /// `CTRL-L` (whole-line, current buffer) is in scope this slice — the frontend intercepts it before
    /// `feed_insert` (it needs the buffer) and resolves the candidate lines. Any other key reaching the
    /// engine cancels the submode. A one-key expectation local to Insert; ALWAYS separate from the Emacs
    /// profile's `C-x` prefix (`emacs_prefix`), which lives on the non-modal `feed` path.
    pub(super) ctrl_x: bool,
    /// Insert resumed after an `i_CTRL-O` one-shot ended the previous change (#795): the next edit opens
    /// a fresh `i` dot-repeat change. Vim's `arrow_used`/`stop_arrow` pair. Local to Insert.
    pub(super) resumed: bool,
    /// `CTRL-G U` was typed: the next `<Left>`/`<Right>` stays inside the undo group and the `.` record
    /// ([`Command::InsertCaretStep`]). Any key consumes it. Local to Insert.
    pub(super) keep_undo: bool,
    /// The char the previous Insert key typed (the printable-insert path), for `0 CTRL-D` / `^ CTRL-D`. Any
    /// other key clears it. Local to Insert.
    pub(super) last_typed: Option<char>,
}

/// One in-flight insert-mode keyword-completion session (`i_CTRL-N` / `i_CTRL-P`). The candidate list is
/// buffer-derived (resolved once by the frontend when the cycle starts) and cached here across keys; the
/// engine owns only the CYCLING (the index), mirroring how `i_CTRL-E` splits buffer-read (frontend) from
/// command-emission (engine). The cycle has `cands.len() + 1` stops: the candidates in scan order, then
/// the ORIGINAL typed text (Vim's "Back at original"). `C-N` advances the index, `C-P` retreats it, both
/// modulo the cycle length. `applied` is the char length of the text currently occupying the completion
/// region in the buffer (the base, or the last applied candidate) — the `back` count the next step deletes.
pub(super) struct Completion {
    /// The keyword text before the caret when the cycle started (the "original" cycle stop / base).
    pub(super) base: String,
    /// Candidates in nvim scan order (forward-from-caret then wrap, deduped) — see `keyword_completions`.
    pub(super) cands: Vec<String>,
    /// Current cycle position: `0..cands.len()` selects a candidate; `cands.len()` is the ORIGINAL text.
    pub(super) idx: usize,
    /// Char length of the text currently in the buffer's completion region (what the next step deletes).
    pub(super) applied: u32,
}

impl Completion {
    /// The text at cycle position `idx`: a candidate, or the original base at the wrap-around stop.
    pub(super) fn text_at(&self, idx: usize) -> &str {
        self.cands
            .get(idx)
            .map_or(self.base.as_str(), String::as_str)
    }
}

/// The `i_CTRL-K` digraph collector's position within its two-char sequence (see [`InsertState::digraph`]).
#[derive(Clone, Copy)]
pub(super) enum DigraphPending {
    /// Armed by `CTRL-K`; the next printable key is the first code char.
    First,
    /// The first code char is in hand; the next printable key completes the digraph.
    Second(char),
}

/// The `i_CTRL-V` literal-entry collector's position (see [`InsertState::literal`]).
#[derive(Clone, Copy)]
pub(super) enum LiteralEntry {
    /// `CTRL-V` seen; the next key selects a numeric form (a decimal digit, or `o`/`O`/`x`/`X`/`u`/`U`)
    /// or, being none of those, is inserted literally.
    AwaitFirst,
    /// Mid numeric collection in `base`: `value` accumulated so far, `remaining` digits still allowed, and
    /// `count` digits collected. Resolves when `remaining` reaches 0 or a non-digit key terminates it.
    Collecting {
        base: LiteralBase,
        value: u32,
        remaining: u8,
        count: u8,
    },
}

/// The numeric base of an in-flight [`LiteralEntry::Collecting`] (`i_CTRL-V`). The per-form digit cap
/// lives on the `remaining` budget, not here (`x`/`X`=2, `u`=4, `U`=8 all share [`LiteralBase::Hex`]).
#[derive(Clone, Copy)]
pub(super) enum LiteralBase {
    /// Decimal (`CTRL-V {ddd}`) — up to 3 digits, value clamped to a single byte (255).
    Dec,
    /// Octal (`CTRL-V o{ooo}` / `O{ooo}`) — up to 3 digits, value clamped to a single byte (255).
    Oct,
    /// Hex — `CTRL-V x`/`X` (≤2 digits), `u` (≤4, BMP) or `U` (≤8, full Unicode).
    Hex,
}

impl LiteralBase {
    pub(super) fn radix(self) -> u32 {
        match self {
            LiteralBase::Dec => 10,
            LiteralBase::Oct => 8,
            LiteralBase::Hex => 16,
        }
    }
}
