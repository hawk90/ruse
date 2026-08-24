//! The input engine: a small pending-state machine that folds keys into semantic [`Command`]s
//! (`d`, `2w`, `d3w`, `dd`, `cw`=`ce`), plus ex-command (`:…`) parsing. The trace records the resulting
//! commands, so re-keymapping never invalidates a corpus. Pure and unit-tested.

use std::collections::HashMap;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ruse_core::keymap::{Resolved, UnmatchedKey};
use ruse_core::{
    BlockInsertKind, Command, EditorOption, ForcedWise, GlobalCmd, LineAddr, MarkOp, Mode, Motion,
    OpKind, SearchOffset, SearchOp, SelectKind, SubFlags, SubRange, WordCase,
};

/// The outcome of feeding one key to the engine.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Feed {
    /// A completed command to apply.
    Cmd(Command),
    /// `.` (dot-repeat): replay a recorded change as an ORDERED command list — the change's leading
    /// command followed by any insert-session text (F-023). The frontend applies each in turn, exactly as
    /// if the original keys were re-typed at the current cursor. Kept distinct from `Cmd` because one
    /// keypress expands to a compound edit, and because the driver must NOT re-record it as a new change.
    Replay(Vec<Command>),
    /// A completed `:`-line to execute (F-026). The command-line namespace owns the buffer while it is
    /// being typed; on `<CR>` it hands the finished text to the frontend to parse+run as an ex command.
    ExecuteEx(String),
    /// `c_CTRL-R_CTRL-W` / `c_CTRL-R_CTRL-A`: splice the `<cword>` (`big == false`) or `<cWORD>`
    /// (`big == true`) under the BUFFER cursor into the open command line at the caret. The namespace owns
    /// the cmdline buffer but the engine has no document, so the frontend resolves the word (via
    /// `Workspace::cword_under_cursor` / `cbig_word_under_cursor`) and splices it back with
    /// [`InputEngine::cmdline_splice`]. The command line stays open.
    CmdlineInsertUnder { big: bool },
    /// `!{motion}` / `!!` (the Normal-mode filter operator): the motion resolved to a LINE range, but the
    /// engine has no document to number it and no shell to run. The frontend resolves the motion's first/last
    /// line (via `Workspace::reindent_range`), then opens the `:` command-line pre-seeded with
    /// `{first},{last}!` ([`InputEngine::open_filter_cmdline`]); typing the command and pressing `<CR>` runs
    /// the SAME `:{range}!{cmd}` ex filter. `count` is the folded operator × motion count.
    FilterMotion { count: u32, motion: Motion },
    /// The key was consumed but the command is not complete yet (a count digit, a pending operator, or a
    /// keystroke absorbed into the open command-line buffer).
    Pending,
    /// Nothing bound.
    Ignored,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Op {
    Delete,
    Change,
    Yank,
    /// `gu` — lowercase the operator span. `gU` = upper, `g~` = toggle, `g?` = ROT13.
    CaseLower,
    CaseUpper,
    CaseToggle,
    Rot13,
    /// `>` / `<` — indent the operator span's lines right / left (linewise).
    ShiftRight,
    ShiftLeft,
    /// `=` — reindent the operator span's lines to their bracket depth (linewise).
    Reindent,
    /// `gq` / `gw` — reflow the operator span's lines to `'textwidth'`. `Format` moves the caret to the
    /// last reformatted line; `FormatKeep` (`gw`) restores it.
    Format,
    FormatKeep,
    /// `!` — filter the operator span's LINES through a shell command (`!{motion}{cmd}<CR>`, `!!{cmd}`).
    /// Unlike the other operators this does not resolve to a buffer edit in the engine: the completed
    /// motion yields a [`Feed::FilterMotion`] the frontend turns into a pre-seeded `:{range}!` ex line
    /// (reusing the `:{range}!{cmd}` executor + `shell.rs`).
    Filter,
}

impl Op {
    /// The `WordCase` a case operator applies (`None` for the non-case operators).
    fn case(self) -> Option<WordCase> {
        match self {
            Op::CaseLower => Some(WordCase::Downcase),
            Op::CaseUpper => Some(WordCase::Upcase),
            Op::CaseToggle => Some(WordCase::Toggle),
            Op::Rot13 => Some(WordCase::Rot13),
            _ => None,
        }
    }

    /// Whether this is a shift operator, and its direction (`Some(true)` = left, `Some(false)` = right).
    fn shift(self) -> Option<bool> {
        match self {
            Op::ShiftLeft => Some(true),
            Op::ShiftRight => Some(false),
            _ => None,
        }
    }
}

/// The operator-pending axis: an armed operator (`d`/`c`/`y`) plus the count that preceded it.
#[derive(Clone, Copy, PartialEq, Eq)]
struct OpPending {
    op: Op,
    count: u32,
}

/// The one-shot key-expectation axis — what the *next* key must supply. Held separately from the operator
/// and count axes (per input-engine.md, these are **orthogonal** and must not be crammed into one enum).
/// Exactly one variant holds between keystrokes, so illegal combinations (awaiting a find-target *and* a
/// text-object char at once) are unrepresentable — the class of hierarchy bug that ad-hoc booleans invite.
#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum Awaiting {
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
    /// After `` ` `` (backtick): the next key names a mark to jump to — `.` (last change) or a named mark
    /// `a`–`z`. Any other key aborts.
    MarkJump,
    /// After `m`: the next key names the mark (`a`–`z`) to SET at the cursor. Any other key aborts.
    SetMarkChar,
    /// After `'` (apostrophe): the next key names a mark to jump to LINEWISE (first non-blank of its line) —
    /// `.` (last change) or `a`–`z`. Any other key aborts.
    MarkJumpLine,
    /// After `"`: the next key is the register NAME (`a`–`z`, or `A`–`Z` to append). It arms a one-shot
    /// pending register that the FOLLOWING yank/delete/change/paste targets — emitted as a
    /// [`Command::SetRegister`] the core applies before that command. `"` itself does not reset the count.
    RegisterSelect,
    /// After `]` or `[`: the next key selects a bracket command. Only the indent-adjusting pastes are wired
    /// so far — `]p`/`]P`/`[p`/`[P` ([`Command::PasteIndent`]). `open` records which bracket started it (`]`
    /// pastes below like `p`, `[` above like `P`, before the second key's own `p`/`P` refines it).
    BracketPrefix { open_bracket: bool },
}

/// The Normal-grammar layer's OWNED state (KL-OBL-4): the three orthogonal transient axes of the
/// Normal / Visual / Select / Operator-pending family, which share one grammar. It is a field of the
/// engine, but it BELONGS to that layer family — it is dropped (`NormalState::default()`) the instant
/// the active namespace leaves the family (into Insert / Replace / Cmdline / Terminal), so the count
/// or armed operator can never survive into another layer. The engine no longer reaches in to reset
/// individual axes on a mode change; the layer's state dies with the layer.
#[derive(Default)]
struct NormalState {
    /// Count axis: the accumulating numeric prefix for the next motion/operator.
    count: u32,
    /// Operator axis: an armed operator awaiting its motion (`None` = none).
    op: Option<OpPending>,
    /// Key-expectation axis: what the next key must supply (the top-priority resolution tier).
    awaiting: Awaiting,
    /// A forced motion wise armed by `v`/`V` after an operator (Vim `o_v`/`o_V`): the NEXT motion
    /// resolves into a [`Command::OpForced`] instead of a plain operator command. `None` unless
    /// mid-`dv…`/`dV…`.
    forced_wise: Option<ForcedWise>,
}

impl NormalState {
    /// A PRISTINE Normal base: no count, no armed operator, no key-expectation, no forced wise. The Native
    /// leader tier (F-013 NAT-2) only arms from here, so a Space mid-construct (`d<Space>`, `2<Space>`)
    /// stays the Vim right-motion — the text grammar is untouched (NAT-1).
    fn is_clean(&self) -> bool {
        self.count == 0
            && self.op.is_none()
            && self.awaiting == Awaiting::Nothing
            && self.forced_wise.is_none()
    }
}

/// A SUSPENDED layer awaiting return (KL-OBL-5): while a one-shot command is borrowed to run in
/// another namespace, this records the ADDRESS to resume — *whence* control came. `i_CTRL-O` suspends
/// Insert to run one Normal command (`resume: Insert`); `t_CTRL-\ CTRL-O` (deferred, no terminal
/// buffers yet) is the SAME construct with `resume: Terminal`. A flat boolean edge cannot record
/// whence; a stack of these can, and nests for free.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Suspended {
    /// The namespace to resume once the borrowed one-shot command completes.
    resume: Ns,
}

/// The Insert layer's OWNED transient state (KL-OBL-4): the one-key `CTRL-G` prefix local to Insert.
/// Dropped (`InsertState::default()`) when the active namespace is not Insert — the layer's state dies
/// with the layer. (The `i_CTRL-O` one-shot is no longer a bool here; its RETURN ADDRESS lives on the
/// engine's activation stack — KL-OBL-5 — because a return spans two layers, not one.)
#[derive(Default)]
struct InsertState {
    /// Insert-mode `CTRL-G` prefix: the next key is expected to be `u` (undo-break,
    /// [`Command::BreakUndo`]). A one-key expectation local to Insert; any other second key aborts it.
    ctrl_g: bool,
    /// Insert-mode `CTRL-R` prefix: the next key is the register NAME whose contents to insert at the caret
    /// ([`Command::InsertRegister`]). A one-key expectation local to Insert; a non-register key aborts it.
    ctrl_r: bool,
    /// Insert-mode `CTRL-K` digraph prefix (`i_CTRL-K`): a TWO-key expectation. `None` = inactive;
    /// `Some(DigraphPending::First)` = armed, awaiting the first of the two code chars;
    /// `Some(DigraphPending::Second(c1))` = first char collected, awaiting the second. Local to Insert.
    digraph: Option<DigraphPending>,
    /// Insert-mode `CTRL-V` literal / numeric-code entry (`i_CTRL-V`): `None` = inactive;
    /// `Some(LiteralEntry::AwaitFirst)` = `CTRL-V` seen, the next key picks a numeric form or is inserted
    /// literally; `Some(LiteralEntry::Collecting{..})` = mid numeric collection. Local to Insert.
    literal: Option<LiteralEntry>,
    /// Insert-mode keyword completion (`i_CTRL-N` / `i_CTRL-P`): the in-flight cycle across successive
    /// `CTRL-N`/`CTRL-P` keys. `None` = no active completion. Cleared when ANY other key is fed in Insert
    /// (that key ACCEPTS the current candidate, Vim's behavior) and, like the rest of `InsertState`, when
    /// the Insert layer dies. Local to Insert.
    completion: Option<Completion>,
}

/// One in-flight insert-mode keyword-completion session (`i_CTRL-N` / `i_CTRL-P`). The candidate list is
/// buffer-derived (resolved once by the frontend when the cycle starts) and cached here across keys; the
/// engine owns only the CYCLING (the index), mirroring how `i_CTRL-E` splits buffer-read (frontend) from
/// command-emission (engine). The cycle has `cands.len() + 1` stops: the candidates in scan order, then
/// the ORIGINAL typed text (Vim's "Back at original"). `C-N` advances the index, `C-P` retreats it, both
/// modulo the cycle length. `applied` is the char length of the text currently occupying the completion
/// region in the buffer (the base, or the last applied candidate) — the `back` count the next step deletes.
struct Completion {
    /// The keyword text before the caret when the cycle started (the "original" cycle stop / base).
    base: String,
    /// Candidates in nvim scan order (forward-from-caret then wrap, deduped) — see `keyword_completions`.
    cands: Vec<String>,
    /// Current cycle position: `0..cands.len()` selects a candidate; `cands.len()` is the ORIGINAL text.
    idx: usize,
    /// Char length of the text currently in the buffer's completion region (what the next step deletes).
    applied: u32,
}

impl Completion {
    /// The text at cycle position `idx`: a candidate, or the original base at the wrap-around stop.
    fn text_at(&self, idx: usize) -> &str {
        self.cands
            .get(idx)
            .map_or(self.base.as_str(), String::as_str)
    }
}

/// The `i_CTRL-K` digraph collector's position within its two-char sequence (see [`InsertState::digraph`]).
#[derive(Clone, Copy)]
enum DigraphPending {
    /// Armed by `CTRL-K`; the next printable key is the first code char.
    First,
    /// The first code char is in hand; the next printable key completes the digraph.
    Second(char),
}

/// The `i_CTRL-V` literal-entry collector's position (see [`InsertState::literal`]).
#[derive(Clone, Copy)]
enum LiteralEntry {
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
enum LiteralBase {
    /// Decimal (`CTRL-V {ddd}`) — up to 3 digits, value clamped to a single byte (255).
    Dec,
    /// Octal (`CTRL-V o{ooo}` / `O{ooo}`) — up to 3 digits, value clamped to a single byte (255).
    Oct,
    /// Hex — `CTRL-V x`/`X` (≤2 digits), `u` (≤4, BMP) or `U` (≤8, full Unicode).
    Hex,
}

impl LiteralBase {
    fn radix(self) -> u32 {
        match self {
            LiteralBase::Dec => 10,
            LiteralBase::Oct => 8,
            LiteralBase::Hex => 16,
        }
    }
}

/// The active input profile (F-012 / RFC-0014, F-013 / RFC-0016). Vim is a MODAL grammar (Normal/Insert/
/// Visual, operator-pending); Emacs is NON-MODAL (always editable, `C-` bindings are commands); Native is
/// the third language, whose TEXT layer REUSES the Vim modal grammar (NAT-1) and layers command-discovery
/// (leader/which-key, NAT-2), transient special-view maps (NAT-3) and a readline line (NAT-4) on top. The
/// profiles dispatch differently, so `feed` branches on this before any modal handling — they are not two
/// keymaps over one state machine. Only Emacs takes the non-modal path; Vim and Native share the modal path
/// (Native's distinctive layers are additive, they do not replace the text grammar). `input.profile`
/// (config-schema) selects it; no config loader exists yet, so it is set at construction (`InputEngine::new`
/// = Vim, `::emacs` = Emacs, `::native` = Native).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum InputProfile {
    Vim,
    Emacs,
    /// The Native profile (F-013). Modal text = the Vim grammar (NAT-1); command discovery, transient
    /// action maps and a readline line layer on additively. In this slice it is behaviourally Vim plus the
    /// distinct identity — the leader/which-key tier (NAT-2) et al. land in following slices.
    Native,
}

/// The Normal/Visual input state, held as three **orthogonal axes** — `count`, the operator-pending `op`,
/// and the one-shot `awaiting` key-expectation — plus sticky repeat state. `feed` resolves them in a fixed
/// precedence (mode → awaiting tier → base keys), so the hierarchy is explicit, not encoded in field order.
pub struct InputEngine {
    /// Which input profile is active — Vim (modal grammar) or Emacs (non-modal). Chosen at construction.
    input_profile: InputProfile,
    /// The active profile's layers. Built once — resolution must not allocate per keystroke.
    profile: VimProfile,
    /// The Normal-family grammar layer's owned transient state (count / operator / awaiting / forced
    /// wise). Dropped when the family deactivates — KL-OBL-4.
    normal: NormalState,
    /// Sticky (survives command completion): the last char-search `(ch, forward, till)`, for `;`/`,`.
    last_find: Option<(char, bool, bool)>,
    /// Sticky: the last search `(pattern, forward, offset)`, for `n`/`N`. `forward` records the DIRECTION
    /// the search was issued in (`/` = true, `?` = false) so `n` repeats in the SAME direction and `N` in
    /// the OPPOSITE. `offset` is the [`SearchOffset`] the search carried (`:help search-offset`); `n`/`N`
    /// reapply it by re-issuing a [`Command::Search`] with the stored offset rather than a plain
    /// [`Command::SearchNext`]/[`Command::SearchPrev`], so `/foo/e` then `n` lands on the next match's end.
    last_search: Option<(String, bool, SearchOffset)>,
    /// Sticky: the last completed change, replayed by `.` (dot-repeat). `None` until the first change, so
    /// a bare `.` before any edit is a clean no-op.
    last_change: Option<ChangeIntent>,
    /// An in-flight change being recorded: set when an insert-entering command fires, then extended with the
    /// insert-session commands until the terminating `<Esc>` (`EnterNormal`) closes it into `last_change`.
    recording: Option<ChangeIntent>,
    /// The typed body of the MOST RECENT closed insert session (`i_CTRL-A` / `i_CTRL-@`; Vim's `".`
    /// register), excluding the terminating `EnterNormal`. Kept SEPARATE from `last_change` because a
    /// non-insert change (`x`, `dw`) overwrites `last_change` but must NOT touch the last-inserted text
    /// (verified vs nvim). Empty until the first insert session closes, so `i_CTRL-A` before any insert is
    /// a clean no-op. Updated whenever an insert session closes, so text re-inserted by `i_CTRL-A` itself
    /// (captured into the in-flight recording) rolls into the next `".` (matches nvim).
    last_inserted: Vec<Command>,
    /// The register named by the most recent `"x`, held only until the NEXT recorded change picks it up (so
    /// `.` replays it). Cleared by any intervening non-register command — a stray `"x` then a motion forgets.
    pending_record_register: Option<char>,
    /// The count typed before a PURE insert-entry (`3i`/`3o`/`3A`; VIM-CNT-INS), captured BEFORE
    /// [`action`](Self::action) resets the axes and handed to the [`ChangeIntent`] the next insert-entry
    /// opens. `0` between entries; only the six pure insert-entries set it (change-family entries leave it
    /// clear, so their count never repeats text). See [`InputEngine::insert_entry`].
    pending_insert_count: u32,
    /// The count-on-insert replication tail (VIM-CNT-INS) produced when a count-prefixed insert session
    /// closes on `<Esc>`: the `(count - 1)` extra repeats of the typed text plus the `EnterNormal` that
    /// leaves Insert. [`feed`](Self::feed) takes it and returns it as a [`Feed::Replay`] in place of the
    /// bare `EnterNormal`, so the repeats apply as ONE undo group (consecutive edits coalesce). `None`
    /// for a count-less insert and for every non-insert command.
    pending_insert_replay: Option<Vec<Command>>,
    /// The operator+count armed when `/` opened the search line, held across the minibuffer's pattern entry
    /// so `submit_search` can fold them into the finished command (`d/pat`, `2/pat`). `/` captures this
    /// BEFORE `reset()` wipes the axes; `None` between searches. See [`InputEngine::submit_search`].
    pending_search: Option<(SearchOp, u32)>,
    /// The Insert layer's owned transient state (the `CTRL-G` prefix). Dropped when the active
    /// namespace is not Insert — KL-OBL-4.
    insert: InsertState,
    /// The activation stack (KL-OBL-5): return ADDRESSES for one-shot commands one layer borrows to
    /// run in another. `i_CTRL-O` pushes `Suspended{resume: Insert}`; the completing command pops it,
    /// resuming Insert. Empty in steady state. A stack rather than a bool so it records *whence* and
    /// nests for free (the general form; `i_CTRL-O` is its depth-1 case, `t_CTRL-\ CTRL-O` its second).
    activations: Vec<Suspended>,
    /// The command-line namespace (F-026): while `Some`, keys are routed into its owned line buffer
    /// rather than the Normal grammar. `None` = not on the command line. This is the engine owning the
    /// line, not an ad-hoc text buffer on the UI (anti-pattern command-line P2).
    cmdline: Option<CmdLine>,
    /// The command-line window (`:help cmdwin`, `q:`/`q/`/`q?`): `Some` while the history list-overlay owns
    /// the keystream. A NAVIGABLE read-only slice of the reduced port (in-window editing is deferred pending
    /// a hostable secondary editable buffer); `<CR>` runs the selected line through the same ex/search
    /// dispatch the `:`/`/` prompt uses. Mutually exclusive with `cmdline` (opened only from clean Normal).
    cmdwin: Option<CmdWin>,
    /// The `:` (ex) command-line history ring (`:help cmdline-history`): accepted ex lines, most-recent
    /// last, recalled by `<Up>`/`<C-p>` in the prompt. Session-scoped frontend state — SEPARATE from the
    /// search ring (Vim keeps `:` and `/` histories apart), and never in `crates/core`.
    ex_history: CmdHistory,
    /// The `/`+`?` search-pattern history ring (`:help cmdline-history`): `/` and `?` SHARE one ring in
    /// Vim, kept apart from the ex ring above.
    search_history: CmdHistory,
    /// The active Lang-Arg language map (`lmap`, F-027): a char→char rewrite applied by the pre-dispatch
    /// translation stage. Populated by `:lmap` at runtime (the persistent form is `keymap.lang`; no
    /// config-file loader exists for any `keymap.*` key yet). MVP restricts both sides to a single char.
    lang_map: HashMap<char, char>,
    /// Whether the language map is currently active (Vim `iminsert`/`imsearch`, toggled by `i_CTRL-^`).
    /// One boolean for MVP (RFC-0013); the per-context iminsert/imsearch model is a follow-up. `false`
    /// by default so a configured map never silently rewrites the command line you type to define it.
    lang_active: bool,
    /// The Emacs prefix argument being read (F-012 / D-049): `Some` while `C-u`/digits accumulate an
    /// argument that the NEXT command consumes. `None` in steady state and always `None` under the Vim
    /// profile (that profile folds the count into its own grammar). The value is held OPAQUE — each
    /// command decides how to fold it (motions multiply); this is the raw channel D-049 resolved.
    emacs_arg: Option<EmacsArg>,
    /// The pending Emacs prefix key (F-012): `Some('x')` after `C-x`, so the NEXT key resolves inside that
    /// prefix's map (`C-x C-s` = save) rather than the global map. `None` in steady state. This is the
    /// depth-1 case of the multi-key dispatch the nine-tier stack generalises; more prefixes (`C-c`, `C-h`)
    /// slot in by tag. Always `None` under the Vim profile.
    emacs_prefix: Option<char>,
    /// The Emacs profile's nine-tier keymap (F-012 / D-045). Built once; consulted only on the Emacs path.
    /// Present regardless of profile, mirroring `profile: VimProfile` — both are cheap to build.
    emacs: EmacsProfile,
    /// The Native profile's leader (which-key) tier is ARMED (F-013 NAT-2): `<leader>` (Space) was pressed
    /// from a clean Normal base, so the NEXT key resolves in the leader map rather than the Vim grammar.
    /// `false` in steady state and ALWAYS `false` under the Vim/Emacs profiles (only the Native+Normal path
    /// ever sets it). The depth-1 case of the discovery tier; nested groups (`<leader>g …`) layer on by tag.
    leader: bool,
}

/// A short human label for a key, for the palette's binding column (F-004 #2).
fn key_label(code: KeyCode) -> String {
    match code {
        KeyCode::Esc => "Esc".into(),
        KeyCode::Enter => "CR".into(),
        KeyCode::Backspace => "BS".into(),
        KeyCode::Tab => "Tab".into(),
        KeyCode::Char(c) => c.to_string(),
        other => format!("{other:?}"),
    }
}

impl InputEngine {
    #[must_use]
    pub fn new() -> InputEngine {
        Self::with_profile(InputProfile::Vim)
    }

    /// An Emacs-profile engine (F-012). Same state; `feed` takes the non-modal Emacs path.
    #[must_use]
    pub fn emacs() -> InputEngine {
        Self::with_profile(InputProfile::Emacs)
    }

    /// A Native-profile engine (F-013). Same state; `feed` takes the MODAL path — Native's text layer is the
    /// Vim grammar (NAT-1). Its distinctive command-discovery/transient/readline layers are additive follow-
    /// ups, so in this slice a Native engine drives text exactly as Vim does, under its own profile identity.
    #[must_use]
    pub fn native() -> InputEngine {
        Self::with_profile(InputProfile::Native)
    }

    #[must_use]
    fn with_profile(input_profile: InputProfile) -> InputEngine {
        InputEngine {
            input_profile,
            profile: VimProfile::new(),
            normal: NormalState::default(),
            last_find: None,
            last_search: None,
            last_change: None,
            recording: None,
            last_inserted: Vec::new(),
            pending_record_register: None,
            pending_insert_count: 0,
            pending_insert_replay: None,
            pending_search: None,
            insert: InsertState::default(),
            activations: Vec::new(),
            cmdline: None,
            cmdwin: None,
            ex_history: CmdHistory::new(history::DEFAULT_CAP),
            search_history: CmdHistory::new(history::DEFAULT_CAP),
            lang_map: HashMap::new(),
            lang_active: false,
            emacs_arg: None,
            emacs_prefix: None,
            emacs: EmacsProfile::new(),
            leader: false,
        }
    }

    /// Whether a one-shot command borrowed from another layer is in flight (`i_CTRL-O`): keys resolve
    /// through the Normal grammar until it completes and pops its return address (KL-OBL-5).
    fn in_one_shot(&self) -> bool {
        !self.activations.is_empty()
    }

    /// A human label for `command`'s STATIC keymap-layer binding (F-004 #2 palette column), or `None`
    /// if it is not bound in any namespace's layer table. Grammar-driven commands (motions, operators
    /// built by `feed`, not the layer tables) and ex commands have no static binding here — the
    /// deliberate MVP scope (static layer bindings only), so most Normal commands return `None`.
    #[must_use]
    pub fn binding_label(&self, command: &Command) -> Option<String> {
        for ns in VimProfile::all()
            .into_iter()
            .chain(std::iter::once(Ns::Replace))
        {
            if let Some(code) = self.profile.stack(ns).key_for(command) {
                return Some(key_label(*code));
            }
        }
        None
    }

    /// The active command-line as `(prefix, text, cursor)` for the frontend to render — `None` when
    /// not on the command line. The frontend reads this instead of owning any line buffer (F-026).
    #[must_use]
    pub fn cmdline(&self) -> Option<(char, &str, usize)> {
        self.cmdline
            .as_ref()
            .map(|c| (c.prefix, c.buffer.as_str(), c.cursor))
    }

    /// Splice `text` into the open command line at the caret — the frontend's response to
    /// [`Feed::CmdlineInsertUnder`] (`c_CTRL-R_CTRL-W`/`_CTRL-A`), after it has resolved the `<cword>` /
    /// `<cWORD>` from the buffer. No-op when no line is open. MVP appends at the end (the cmdline caret is
    /// end-anchored, matching the append/backspace edit model); splicing also ends any history-recall walk.
    pub fn cmdline_splice(&mut self, text: &str) {
        if let Some(cl) = self.cmdline.as_mut() {
            cl.buffer.push_str(text);
            cl.cursor = cl.buffer.chars().count();
            cl.walk = history::HistWalk::default();
        }
    }

    /// The Native leader (which-key) discovery hint (F-013 NAT-2) as a one-line `"w:write  q:quit  …"`
    /// string for the status/command line, or `None` unless the leader tier is armed. `Some` iff
    /// `<leader>` is pending, so it doubles as the pending-state query; formatting lives here so the
    /// frontend stays a thin renderer. A structured multi-column which-key popup is a later render slice.
    #[must_use]
    pub fn leader_hint(&self) -> Option<String> {
        if !self.leader {
            return None;
        }
        Some(
            NATIVE_LEADER_MENU
                .iter()
                .map(|(k, label, _)| format!("{k}:{label}"))
                .collect::<Vec<_>>()
                .join("  "),
        )
    }

    /// Open the command-line namespace with `prefix` (`:`/`/`), optionally as `gQ` Ex mode.
    fn open_cmdline(&mut self, prefix: char, ex_mode: bool) {
        self.cmdline = Some(CmdLine {
            prefix,
            buffer: String::new(),
            cursor: 0,
            ex_mode,
            mx: false,
            expr: None,
            ctrl_r: false,
            walk: history::HistWalk::default(),
        });
    }

    /// Open the `:` command-line PRE-SEEDED with `seed` and the caret at its end — the landing point of the
    /// Normal-mode `!{motion}` / `!!` filter operator. The frontend resolves the motion's line range and
    /// seeds `"{first},{last}!"`, so typing the shell command and pressing `<CR>` runs the SAME
    /// `:{range}!{cmd}` ex filter (reusing its executor + `shell.rs`). Vim shows this pre-filled `:.,.+N!`.
    pub fn open_filter_cmdline(&mut self, seed: &str) {
        self.cmdline = Some(CmdLine {
            prefix: ':',
            buffer: seed.to_string(),
            cursor: seed.chars().count(),
            ex_mode: false,
            mx: false,
            expr: None,
            ctrl_r: false,
            walk: history::HistWalk::default(),
        });
    }

    /// Open the command-line window (`:help cmdwin`) for `kind` — `:` mirrors the ex ring, `/`/`?` the
    /// shared search ring. The frontend calls this when `q:`/`q/`/`q?` is recognised (the macro layer routes
    /// the `q`-prefixed `:`/`/`/`?` here instead of arming a macro recording). No-op if already open.
    pub fn open_cmdwin(&mut self, kind: char) {
        let ring = if kind == '/' || kind == '?' {
            &self.search_history
        } else {
            &self.ex_history
        };
        self.cmdwin = Some(CmdWin::open(kind, ring.entries_ref()));
    }

    /// The open command-line window's kind glyph (`:`/`/`/`?`), or `None` when closed. The frontend reads it
    /// for the prompt glyph and to route keys / suppress Normal-mode intercepts while the overlay owns them.
    #[must_use]
    pub fn cmdwin(&self) -> Option<char> {
        self.cmdwin.as_ref().map(|c| c.kind)
    }

    /// The command-line window's visible list rows `(text, is_selected)` for the overlay paint slot, or an
    /// empty vec when closed. The frontend paints these in the same slot the pickers use.
    #[must_use]
    pub fn cmdwin_rows(&self) -> Vec<(String, bool)> {
        self.cmdwin.as_ref().map(CmdWin::rows).unwrap_or_default()
    }

    /// Route one key into the open command-line window (`:help cmdwin`). `j`/`<Down>` and `k`/`<Up>` move the
    /// selection; `<CR>` runs the selected line — an ex line becomes [`Feed::ExecuteEx`], a search folds
    /// through [`Self::submit_search`] — and closes the window (an empty line runs nothing); `<Esc>`/`<C-c>`
    /// closes without running. Any other key is swallowed (the overlay is modal). Accepting a line records it
    /// in the matching ring, exactly as the `:`/`/` prompt does on `<CR>`.
    fn feed_cmdwin(&mut self, key: KeyEvent) -> Feed {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let Some(cw) = self.cmdwin.as_mut() else {
            return Feed::Ignored;
        };
        match key.code {
            KeyCode::Char('c') if ctrl => {
                self.cmdwin = None;
                Feed::Ignored
            }
            KeyCode::Esc => {
                self.cmdwin = None;
                Feed::Ignored
            }
            KeyCode::Char('j') | KeyCode::Down => {
                cw.down();
                Feed::Pending
            }
            KeyCode::Char('k') | KeyCode::Up => {
                cw.up();
                Feed::Pending
            }
            KeyCode::Char('G') => {
                // Jump to the empty last line (Vim `G`); the list is short, so no count handling.
                while {
                    let before = cw.selected_line().to_string();
                    cw.down();
                    before != cw.selected_line()
                } {}
                Feed::Pending
            }
            KeyCode::Enter => {
                let kind = cw.kind;
                let text = cw.selected_line().to_string();
                self.cmdwin = None;
                if text.is_empty() {
                    return Feed::Ignored; // `<CR>` on the empty line just closes (nothing to run)
                }
                // Run the chosen line through the SAME accept path the `:`/`/` prompt uses on `<CR>`: push it
                // onto the matching ring, then execute (`/`+`?` share the search ring; `:` the ex ring).
                if kind == '/' || kind == '?' {
                    self.search_history.push(&text);
                    self.submit_search(text, kind == '?')
                } else {
                    self.ex_history.push(&text);
                    Feed::ExecuteEx(text)
                }
            }
            _ => Feed::Pending, // modal: swallow everything else
        }
    }

    /// Open the Emacs `M-x` minibuffer (F-012): the same command-line namespace, reading a command NAME.
    /// The prompt glyph is a placeholder here; the frontend shows the `M-x ` prompt (a rendering follow-up).
    fn open_minibuffer(&mut self) {
        self.cmdline = Some(CmdLine {
            prefix: ':',
            buffer: String::new(),
            cursor: 0,
            ex_mode: false,
            mx: true,
            expr: None,
            ctrl_r: false,
            walk: history::HistWalk::default(),
        });
    }

    /// Open the expression-register prompt (`"=` / `<C-r>=`, `:help quote=`): the same command-line namespace,
    /// reading an EXPRESSION. The `=` prefix renders the prompt glyph. On `<CR>` the collected string is handed
    /// to the evaluator via the command `target` produces (see [`ExprTarget`]); `<Esc>` aborts.
    fn open_expr_prompt(&mut self, target: ExprTarget) {
        self.cmdline = Some(CmdLine {
            prefix: '=',
            buffer: String::new(),
            cursor: 0,
            ex_mode: false,
            mx: false,
            expr: Some(target),
            ctrl_r: false,
            walk: history::HistWalk::default(),
        });
    }

    /// Route a key into the open command-line buffer (F-026). The namespace owns the buffer: a printable
    /// key appends (open/append policy), `<BS>` deletes back, `<Esc>` aborts, `<CR>` finalises — a
    /// search folds through [`Self::submit_search`], an ex line becomes [`Feed::ExecuteEx`]. In `gQ` Ex
    /// mode the line re-opens after `<CR>` until `:visual`/`:vi`/an empty line exits.
    fn feed_cmdline(&mut self, key: KeyEvent) -> Feed {
        if self.cmdline.is_none() {
            return Feed::Ignored;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        // `c_CTRL-R` prefix armed: THIS key selects what to splice at the cmdline caret. Checked before the
        // history-recall match below so that when armed, `C-r C-p`/etc. is consumed as the (unsupported)
        // selector rather than triggering `<C-p>` history recall. Only the word-under-cursor variants are
        // wired: `c_CTRL-R_CTRL-W` = `<cword>`, `c_CTRL-R_CTRL-A` = `<cWORD>`; the frontend resolves the
        // buffer text (see [`Feed::CmdlineInsertUnder`]). Any other selector aborts the prefix cleanly
        // (general `C-r{reg}` register insertion into the command line is deferred). :help c_CTRL-R_CTRL-W
        if self.cmdline.as_ref().is_some_and(|cl| cl.ctrl_r) {
            if let Some(cl) = self.cmdline.as_mut() {
                cl.ctrl_r = false;
            }
            return match key.code {
                KeyCode::Char('w') if ctrl => Feed::CmdlineInsertUnder { big: false },
                KeyCode::Char('a') if ctrl => Feed::CmdlineInsertUnder { big: true },
                _ => Feed::Pending, // unsupported selector: swallow, stay on the line
            };
        }
        // `c_CTRL-R` — arm the prefix; the next key selects the word/register to insert.
        if ctrl && key.code == KeyCode::Char('r') {
            if let Some(cl) = self.cmdline.as_mut() {
                cl.ctrl_r = true;
            }
            return Feed::Pending;
        }
        // History recall (`:help cmdline-history`), handled before the buffer borrow so the recall helper
        // can re-borrow the (disjoint) history ring. nvim-VERIFIED distinction: `<Up>`/`<Down>` recall
        // PREFIX-FILTERED by the draft typed before the walk; `<C-p>`/`<C-n>` walk the RAW ring unfiltered.
        match key.code {
            KeyCode::Up => return self.cmdline_recall(true, true),
            KeyCode::Down => return self.cmdline_recall(false, true),
            KeyCode::Char('p') if ctrl => return self.cmdline_recall(true, false),
            KeyCode::Char('n') if ctrl => return self.cmdline_recall(false, false),
            _ => {}
        }
        let Some(cl) = self.cmdline.as_mut() else {
            return Feed::Ignored;
        };
        match key.code {
            KeyCode::Esc => {
                self.cmdline = None;
                Feed::Ignored
            }
            KeyCode::Backspace => {
                cl.buffer.pop();
                cl.cursor = cl.buffer.chars().count();
                // Editing the line ends the current recall walk: the next `<Up>` re-captures the edited
                // buffer as its draft/prefix (Vim recalls against the CURRENT line after an edit).
                cl.walk = history::HistWalk::default();
                Feed::Pending
            }
            KeyCode::Char(c) => {
                cl.buffer.push(c);
                cl.cursor = cl.buffer.chars().count();
                cl.walk = history::HistWalk::default();
                Feed::Pending
            }
            KeyCode::Enter => {
                let prefix = cl.prefix;
                let ex_mode = cl.ex_mode;
                let mx = cl.mx;
                let expr = cl.expr;
                let text = std::mem::take(&mut cl.buffer);
                // Expression-register prompt (`"=` / `<C-r>=`): hand the collected expression to the editor,
                // which evaluates it. `Paste` arms the `"=` register for the next paste; `Insert` splices the
                // result at the caret. An empty/malformed expression degrades in the editor (empty result).
                if let Some(target) = expr {
                    self.cmdline = None;
                    return match target {
                        ExprTarget::Paste => Feed::Cmd(Command::SetExprRegister(text)),
                        ExprTarget::Insert => Feed::Cmd(Command::InsertEval(text)),
                    };
                }
                if mx {
                    // `M-x <name> <CR>`: resolve the command name against the registry; an unknown name is a
                    // no-op (Emacs shows "[No match]"). Completion / history are deferred (F-004).
                    self.cmdline = None;
                    return match emacs_command_by_name(&text) {
                        Some(cmd) => Feed::Cmd(cmd),
                        None => Feed::Ignored,
                    };
                }
                if ex_mode {
                    // Ex mode: `:visual`/`:vi`/empty leaves it; anything else runs and re-prompts.
                    if text.is_empty() || text == "visual" || text == "vi" {
                        self.cmdline = None;
                        return Feed::Ignored;
                    }
                    // A gQ-Ex line is still an ex command: record it in the `:` history (empty ignored by
                    // push). Re-prompting resets the walk via `open_cmdline`'s fresh `CmdLine`... but gQ
                    // stays in the SAME CmdLine, so clear the walk explicitly for the next line.
                    self.ex_history.push(&text);
                    cl.cursor = 0;
                    cl.walk = history::HistWalk::default();
                    return Feed::ExecuteEx(text);
                }
                self.cmdline = None;
                // Push the ACCEPTED line onto the matching ring (`:help cmdline-history`): `/`+`?` share the
                // search ring, `:` uses the ex ring; empty lines are ignored by `push`. Kept separate.
                if prefix == '/' || prefix == '?' {
                    self.search_history.push(&text);
                    self.submit_search(text, prefix == '?')
                } else {
                    self.ex_history.push(&text);
                    Feed::ExecuteEx(text)
                }
            }
            _ => Feed::Pending,
        }
    }

    /// Recall a history entry into the open command-line (`:help cmdline-history`). `prev` walks older
    /// (`<Up>`/`<C-p>`), else more-recent (`<Down>`/`<C-n>`); `filter` prefix-filters by the typed draft
    /// (`<Up>`/`<Down>`) vs the raw ring (`<C-p>`/`<C-n>`). Chooses the ex vs search ring by prefix; the
    /// `M-x`/expression prompts have no history and pass the key through. Rewrites the buffer + cursor.
    fn cmdline_recall(&mut self, prev: bool, filter: bool) -> Feed {
        let Some(cl) = self.cmdline.as_mut() else {
            return Feed::Ignored;
        };
        // Only the Vim `:` ex line and the `/`+`?` search line carry history. The `M-x` minibuffer and the
        // expression-register prompt do not (their own histories are separate follow-ups) — pass through.
        if cl.mx || cl.expr.is_some() {
            return Feed::Pending;
        }
        // Disjoint borrows: `cl` borrows `self.cmdline`; the ring borrows a separate field.
        let ring = if cl.prefix == '/' || cl.prefix == '?' {
            &self.search_history
        } else {
            &self.ex_history
        };
        let recalled = if prev {
            ring.recall_prev(&mut cl.walk, &cl.buffer, filter)
        } else {
            ring.recall_next(&mut cl.walk, &cl.buffer, filter)
        };
        if let Some(text) = recalled {
            cl.buffer = text;
            cl.cursor = cl.buffer.chars().count();
        }
        Feed::Pending
    }

    /// Apply a namespace's declared unmatched-key policy to a key nothing bound.
    ///
    /// This is the replacement for the shared `Feed::Ignored` fallthrough. The behaviour is
    /// deliberately unchanged today — `Ignore` and `Abort` both clear the transient state and yield
    /// `Ignored` — but the DECISION now comes from the layer that was actually consulted instead of
    /// from one arm at the bottom of `feed`. Separating them properly needs KL-OBL-4 (a layer owns
    /// its state and dies with it), which this engine does not model yet: count/operator/awaiting are
    /// still engine-wide, so `reset()` is the only available approximation of "the layer went away".
    fn unmatched(&mut self, ns: Ns, key: KeyEvent) -> Feed {
        let policy = match self.profile.stack(ns).resolve(&key.code) {
            Resolved::Bound { .. } => {
                // Reachable only if a caller routes a bound key here; treat as unhandled rather than
                // guessing, so a wiring mistake shows up instead of silently doing something.
                self.reset();
                return Feed::Ignored;
            }
            Resolved::Unmatched { policy, .. } => policy,
            // An empty stack is a construction bug (see `VimProfile::new`), never a policy.
            Resolved::NoLayer => unreachable!("every Vim namespace declares exactly one layer"),
        };
        match policy {
            UnmatchedKey::Insert => {
                self.reset();
                match key.code {
                    KeyCode::Char(c) => Feed::Cmd(Command::InsertChar(c)),
                    // `open/insert` is about PRINTABLE keys; a non-printable unmatched key does
                    // nothing, which is not the same statement as `closed/ignore`.
                    _ => Feed::Ignored,
                }
            }
            UnmatchedKey::Ignore | UnmatchedKey::Abort => {
                self.reset();
                Feed::Ignored
            }
            UnmatchedKey::ReplaceSelection => {
                self.reset();
                match key.code {
                    // Vim Select: a printable key deletes the selection, inserts the char, enters Insert.
                    // The core (`Command::ReplaceSelection`) performs all three as one edit.
                    KeyCode::Char(c) => Feed::Cmd(Command::ReplaceSelection(c)),
                    // `open/replace-selection` is about PRINTABLE keys; a non-printable unmatched key
                    // does nothing (it is NOT `closed/ignore`, but the observable result here matches).
                    _ => Feed::Ignored,
                }
            }
            // The remaining open policies belong to namespaces this engine does not reach yet
            // (Cmdline/Terminal/Lang). Reaching one means a namespace was wired without its handler,
            // and failing loudly beats inventing a behaviour.
            other => unreachable!("namespace {ns:?} has unimplemented policy {other:?}"),
        }
    }

    /// Open the search command-line for `/` (forward) or `?` (backward). Search is a MOTION, so an armed
    /// operator/count must survive the minibuffer (`d/pat`, `2?pat`): capture them (op-count × pending
    /// count, as `motion()` folds) BEFORE `reset()` clears the axes, then hand off to the command-line
    /// namespace, which collects the pattern and calls [`Self::submit_search`] to build the command.
    fn enter_search(&mut self, backward: bool) -> Feed {
        let op = match self.normal.op {
            Some(OpPending { op, .. }) => match op {
                Op::Delete => SearchOp::Delete,
                Op::Change => SearchOp::Change,
                Op::Yank => SearchOp::Yank,
                // Recase-/shift-to-search (`gu/pat`, `>/pat`) are not modeled; degrade to a plain motion.
                Op::CaseLower
                | Op::CaseUpper
                | Op::CaseToggle
                | Op::Rot13
                | Op::ShiftRight
                | Op::ShiftLeft
                | Op::Reindent
                | Op::Format
                | Op::FormatKeep
                | Op::Filter => SearchOp::Move,
            },
            None => SearchOp::Move,
        };
        let count = match self.normal.op {
            Some(OpPending { count, .. }) => count.max(1) * self.mcount(),
            None => self.mcount(),
        };
        self.pending_search = Some((op, count));
        self.reset();
        self.open_cmdline(if backward { '?' } else { '/' }, false);
        Feed::Pending
    }

    /// Complete a `/pattern` (forward) or `?pattern` (backward) line, called by the command-line namespace
    /// on `<CR>` with `backward` from the prefix. Folds the pattern into the operator/count captured when
    /// `/`/`?` was pressed and yields the finished [`Command::Search`]: bare (`?pat`, `2?pat`) moves, or
    /// `d?pat`/`c?pat`/`y?pat` operates over the exclusive span between cursor and match. Records
    /// `(pattern, direction)` for `n`/`N`. An EMPTY pattern reuses the last search pattern in the CURRENT
    /// direction (Vim's `?<CR>` / `/<CR>`); with no prior search it aborts, dropping the armed operator.
    pub fn submit_search(&mut self, line: String, backward: bool) -> Feed {
        self.cmdline = None; // completing a search closes the command-line namespace (F-026)
        let (op, count) = self.pending_search.take().unwrap_or((SearchOp::Move, 1));
        // Split the raw line into `{pattern}` + `{offset}` on the LAST unescaped delimiter (`/` forward,
        // `?` backward) — a `\/` in the pattern is literal (`:help search-offset`). `off` is `None` when
        // no offset part is present (`/foo`, `/foo/`), which distinguishes "reuse the last offset" from
        // "explicitly no offset" for the empty-pattern repeat below.
        let delim = if backward { '?' } else { '/' };
        let (raw_pattern, off) = split_search_offset(&line, delim);
        // Empty pattern → reuse the last search's PATTERN (Vim: `/<CR>` / `?<CR>` repeat it), adopting THIS
        // line's direction. A new offset on the empty line replaces the stored one; no offset part reuses
        // it (so `//e` re-searches the last pattern with `e`, and `//` repeats pattern AND offset).
        let (pattern, offset) = if raw_pattern.is_empty() {
            match &self.last_search {
                Some((p, _, last_off)) => (p.clone(), off.unwrap_or(*last_off)),
                None => return Feed::Ignored,
            }
        } else {
            (raw_pattern, off.unwrap_or(SearchOffset::None))
        };
        self.last_search = Some((pattern.clone(), !backward, offset));
        Feed::Cmd(Command::Search {
            op,
            count,
            pattern,
            backward,
            offset,
        })
    }

    /// `n` (`opposite = false`) / `N` (`opposite = true`): repeat the last search, REAPPLYING its offset.
    /// `n` keeps the stored direction, `N` flips it. Routed through [`Command::Search`] (carrying the
    /// offset) so the offset-aware match rule advances correctly even when the offset landed the cursor
    /// before the match. No prior search → the key is unmatched (falls through to the outer namespace).
    fn repeat_search(&mut self, opposite: bool, key: KeyEvent) -> Feed {
        match self.last_search.clone() {
            Some((pattern, forward, offset)) => {
                let backward = if opposite { forward } else { !forward };
                self.action(Command::Search {
                    op: SearchOp::Move,
                    count: 1,
                    pattern,
                    backward,
                    offset,
                })
            }
            None => self.unmatched(Ns::Normal, key),
        }
    }

    /// Record `pattern` as the last search (with its `forward` direction) so `n`/`N` repeat it. Used by the
    /// frontend after it resolves a `*`/`#` (word-under-cursor) search, whose pattern is known only once the
    /// buffer is read: `*` is forward, `#` backward. These carry no offset ([`SearchOffset::None`]).
    pub fn set_last_search(&mut self, pattern: String, forward: bool) {
        self.last_search = Some((pattern, forward, SearchOffset::None));
    }

    /// The last search pattern (`/`, `?`, `*`, `#`), or `None` if nothing has been searched yet. The
    /// frontend reads it to resolve an empty `:s//repl/` pattern, which Vim fills from the last search.
    #[must_use]
    pub fn last_search(&self) -> Option<&str> {
        self.last_search.as_ref().map(|(p, _, _)| p.as_str())
    }

    /// The text of the most recent insert session (Vim's `".` register, `:help quote_.`), reconstructed
    /// from the recorded insert body — the same commands `i_CTRL-A` replays. `None` when nothing has been
    /// inserted yet. The body is replayed over an empty string: `InsertChar`/`InsertNewline`/`InsertTab`
    /// append; `DeleteBack` removes the last char (so `abX<BS>cd` yields the net `abcd`, matching what a
    /// `i_CTRL-A` replay produces). Uncommon in-insert edits (`i_CTRL-W`/`i_CTRL-U`/`i_CTRL-R`) are not
    /// modelled here — a documented, minor divergence from nvim's raw-keystroke capture for those cases.
    #[must_use]
    pub fn last_inserted_text(&self) -> Option<String> {
        if self.last_inserted.is_empty() {
            return None;
        }
        let mut out = String::new();
        for cmd in &self.last_inserted {
            match cmd {
                Command::InsertChar('\r') | Command::InsertNewline => out.push('\n'),
                Command::InsertChar(c) => out.push(*c),
                Command::InsertTab => out.push('\t'),
                Command::DeleteBack => {
                    out.pop();
                }
                _ => {}
            }
        }
        Some(out)
    }

    /// End the current Normal-grammar sequence: the Normal-family layer drops its OWN transient state
    /// (count / operator / awaiting / forced-wise) at a command boundary. This is the layer resetting
    /// itself, not the engine reaching into a foreign layer (KL-OBL-4) — sticky repeat state survives.
    /// Every non-`Pending` outcome runs through here, so no partial sequence leaks into the next command.
    fn reset(&mut self) {
        self.normal = NormalState::default();
        // A borrowed one-shot command is consumed the instant it completes (every completion runs
        // through here): pop its return address off the activation stack, resuming the layer it came
        // from (KL-OBL-5). A no-op when no one-shot is in flight (the stack is empty).
        self.activations.pop();
    }

    /// Take the pending Normal count (0 if none) and clear the transient state. For a frontend intercept
    /// (`H`/`M`/`L`, `C-d`, …) that consumes a key the engine would otherwise never see: it reads the
    /// count the digits accumulated and resets, so the count can't leak onto the next command.
    pub fn take_count(&mut self) -> u32 {
        let n = self.normal.count;
        self.reset();
        n
    }

    fn mcount(&self) -> u32 {
        self.normal.count.max(1)
    }

    /// Emit `m` — an operator command if one is armed, else a bare move — then clear the transient state.
    /// The repeat count is the operator count times the motion count (`3d2w` = 6), or the motion count for
    /// a bare move; the composition itself lives in [`op_over_motion`](Self::op_over_motion).
    fn motion(&mut self, m: Motion) -> Feed {
        let total = match self.normal.op {
            Some(OpPending { count, .. }) => count.max(1) * self.mcount(),
            None => self.mcount(),
        };
        self.op_over_motion(m, total)
    }

    /// Compose the pending operator (if any) over motion `m` with an explicit `total`, then clear the
    /// transient state. `total` is the repeat count for most motions, but the ABSOLUTE 1-based TARGET LINE
    /// for [`Motion::GotoLine`] (whose operator range is `[min,max]` of the cursor and target lines). With
    /// no operator armed it is the bare `Move(total, m)`. Shared by [`motion`](Self::motion) (computed
    /// `total`) and [`screen_op`](Self::screen_op) (the frontend-resolved `H`/`M`/`L` target line).
    fn op_over_motion(&mut self, m: Motion, total: u32) -> Feed {
        // `!{motion}` — the filter operator does not resolve to an edit here: it is linewise-over-motion, so
        // hand the motion + folded count to the frontend (it numbers the lines and opens the `:{range}!`
        // cmdline). Early-return before the edit-building match so `Op::Filter` never reaches its arms.
        if matches!(self.normal.op, Some(OpPending { op: Op::Filter, .. })) {
            self.reset();
            return Feed::FilterMotion {
                count: total,
                motion: m,
            };
        }
        let cmd = match self.normal.op {
            Some(OpPending { op, .. }) => {
                // `gu`/`gU`/`g~` recase the operator span (no forced-wise / `cw`->`ce` rewrites apply).
                if let Some(case) = op.case() {
                    Command::CaseMotion {
                        count: total,
                        motion: m,
                        case,
                    }
                } else if let Some(left) = op.shift() {
                    // `>`/`<` over a motion — the planner shifts the motion's LINES (always linewise).
                    Command::ShiftMotion {
                        left,
                        count: total,
                        motion: m,
                    }
                } else if op == Op::Reindent {
                    // `=` over a motion — the planner reindents the motion's LINES (always linewise).
                    Command::Reindent {
                        count: total,
                        motion: m,
                    }
                } else if op == Op::Format || op == Op::FormatKeep {
                    // `gq`/`gw` over a motion — the planner reflows the motion's LINES to textwidth.
                    Command::Format {
                        count: total,
                        motion: m,
                        keep_cursor: op == Op::FormatKeep,
                    }
                } else if let Some(wise) = self.normal.forced_wise {
                    let opk = match op {
                        Op::Delete => OpKind::Delete,
                        Op::Change => OpKind::Change,
                        Op::Yank => OpKind::Yank,
                        // Case ops are handled by the `op.case()` branch above.
                        Op::CaseLower
                        | Op::CaseUpper
                        | Op::CaseToggle
                        | Op::Rot13
                        | Op::ShiftRight
                        | Op::ShiftLeft
                        | Op::Reindent
                        | Op::Format
                        | Op::FormatKeep
                        | Op::Filter => {
                            unreachable!()
                        }
                    };
                    Command::OpForced {
                        op: opk,
                        count: total,
                        motion: m,
                        wise,
                    }
                } else {
                    match op {
                        Op::Delete => Command::Delete(total, m),
                        // Vim `cw`/`cW` do not eat the trailing space and, unlike `ce`, changing a word's
                        // LAST char changes only that char. The core's `change_range` applies that special
                        // case for `WordFwd`/`BigWordFwd`, so pass the motion through unchanged (rewriting to
                        // `WordEnd` here would wrongly jump into the next word from a word-final cursor).
                        Op::Change => Command::Change(total, m),
                        Op::Yank => Command::Yank(total, m),
                        // Case ops are handled by the `op.case()` branch above.
                        Op::CaseLower
                        | Op::CaseUpper
                        | Op::CaseToggle
                        | Op::Rot13
                        | Op::ShiftRight
                        | Op::ShiftLeft
                        | Op::Reindent
                        | Op::Format
                        | Op::FormatKeep
                        | Op::Filter => {
                            unreachable!()
                        }
                    }
                }
            }
            None => Command::Move(total, m),
        };
        self.reset();
        Feed::Cmd(cmd)
    }

    /// The EFFECTIVE count for a viewport screen-motion (`H`/`M`/`L`), WITHOUT consuming any state. When an
    /// operator is armed this is the operator count times the motion count (`3d2H` = 6H → the 6th line from
    /// the top, verified against nvim), the SAME multiplication [`motion`](Self::motion) applies; with no
    /// operator it is the raw pending count (0 = none, which the viewport resolver treats as 1). A frontend
    /// intercept resolves its target line from this, then hands the line to [`screen_op`](Self::screen_op).
    pub fn screen_count(&self) -> u32 {
        match self.normal.op {
            Some(OpPending { count, .. }) => count.max(1) * self.mcount(),
            None => self.normal.count,
        }
    }

    /// Whether an operator (`d`/`c`/`y`/`>`/`=`/`gu`…) is currently armed, WITHOUT consuming it. The `H`/`M`/
    /// `L` frontend intercept peeks this to pick the target line's SCROLLOFF: the bare cursor-motion keeps a
    /// `'scrolloff'` margin from the window edge, but under an operator Vim IGNORES scrolloff — `dH`/`dL`
    /// operate through the TRUE top/bottom visible line (verified against nvim). So the intercept resolves the
    /// line with `scrolloff = 0` when this is true, and the normal margin otherwise.
    pub fn has_op(&self) -> bool {
        self.normal.op.is_some()
    }

    /// The OPERATOR form of the viewport-resolved screen motions `H`/`M`/`L` (`dH`/`yL`/`c2M`/`>H`/`gUL`…):
    /// compose the pending operator (if any) over `GotoLine` to the ABSOLUTE 1-based `line` the frontend
    /// resolved from the viewport, then clear the transient state. These motions are LINEWISE under an
    /// operator (`:help H`), which the core `GotoLine` operator path already gives — the range is `[min,max]`
    /// of the cursor and target lines, so direction does not matter. With NO operator armed this is the bare
    /// cursor move (`Move(line, GotoLine)`), preserving plain `H`/`M`/`L`. The frontend owns the viewport, so
    /// it resolves `line` (honoring any `[count]`, read via [`peek_count`](Self::peek_count)) and passes it in.
    pub fn screen_op(&mut self, line: u32) -> Feed {
        self.op_over_motion(Motion::GotoLine, line)
    }

    /// Whether the Insert namespace is in a PLAIN-TEXT context right now: not mid a `CTRL-G`/`CTRL-R`
    /// prefix, a `CTRL-K` digraph, a `CTRL-V` literal entry, nor an `i_CTRL-O` one-shot. The frontend
    /// checks this before handling `i_CTRL-E`/`i_CTRL-Y`, so those never steal the key a pending prefix
    /// is waiting for (e.g. a `CTRL-R` register name).
    #[must_use]
    pub fn insert_plain_text_ctx(&self) -> bool {
        !self.insert.ctrl_g
            && !self.insert.ctrl_r
            && self.insert.digraph.is_none()
            && self.insert.literal.is_none()
            && !self.in_one_shot()
    }

    /// `i_CTRL-E` / `i_CTRL-Y`: insert the frontend-resolved character directly below / above the caret.
    /// The engine has no buffer, so the frontend resolves the adjacent-line char (see
    /// [`Workspace::adjacent_line_char`](ruse_core::Workspace::adjacent_line_char)) and passes it here;
    /// `None` = no such char (short/absent adjacent line), a Vim no-op. The resulting `InsertChar` is
    /// folded into the in-flight dot-repeat change so `.` replays the RESOLVED LITERAL char (matching
    /// nvim, which repeats the character copied — not a re-resolution against the new line).
    pub fn insert_copy_char(&mut self, ch: Option<char>) -> Feed {
        match ch {
            Some(c) => {
                let out = Feed::Cmd(Command::InsertChar(c));
                self.record(&out, Mode::Insert);
                out
            }
            None => Feed::Ignored,
        }
    }

    /// Whether an insert-mode keyword completion cycle (`i_CTRL-N` / `i_CTRL-P`) is currently active. The
    /// frontend checks this to decide whether a fresh `CTRL-N`/`CTRL-P` STARTS a cycle (resolve the base +
    /// candidates from the buffer) or CONTINUES one (just step the index).
    #[must_use]
    pub fn completion_active(&self) -> bool {
        self.insert.completion.is_some()
    }

    /// Start an insert-mode keyword completion cycle (`i_CTRL-N` / `i_CTRL-P`) and take the first step.
    /// The frontend resolves `base` (the keyword before the caret) and `cands` (the buffer keywords that
    /// start with it, in nvim scan order — see [`Workspace::keyword_completion`](ruse_core::Workspace::keyword_completion));
    /// the engine owns the cycling. With NO candidates this is a no-op (Vim bells; [`Feed::Ignored`]) and
    /// no cycle is armed. Otherwise the cycle stops are `[cands…, ORIGINAL]`; `forward` (`CTRL-N`) selects
    /// the first candidate, `!forward` (`CTRL-P`) the last. Returns the [`Command::CompleteWord`] applying
    /// the step, recorded into the insert session so `.` replays it.
    pub fn complete_start(&mut self, base: String, cands: Vec<String>, forward: bool) -> Feed {
        if cands.is_empty() {
            return Feed::Ignored;
        }
        let applied = base.chars().count() as u32;
        // Start parked on the ORIGINAL stop (`idx == cands.len()`); the first `complete_cycle` moves off it
        // to the first (`CTRL-N`) or last (`CTRL-P`) candidate.
        self.insert.completion = Some(Completion {
            idx: cands.len(),
            base,
            cands,
            applied,
        });
        self.complete_cycle(forward)
    }

    /// Advance (`CTRL-N`) or retreat (`CTRL-P`) the active keyword-completion cycle by one stop and emit the
    /// [`Command::CompleteWord`] that swaps the buffer's completion region to the newly selected text. A
    /// no-op ([`Feed::Ignored`]) if no cycle is active. The step is recorded into the in-flight insert
    /// session (like [`insert_copy_char`](Self::insert_copy_char)), so dot-repeat replays the chain and
    /// reproduces the accepted text literally (matches nvim v0.12.4).
    pub fn complete_cycle(&mut self, forward: bool) -> Feed {
        let Some(c) = self.insert.completion.as_mut() else {
            return Feed::Ignored;
        };
        let len = c.cands.len() + 1; // candidates + the ORIGINAL-text stop
        c.idx = if forward {
            (c.idx + 1) % len
        } else {
            (c.idx + len - 1) % len
        };
        let text = c.text_at(c.idx).to_string();
        let back = c.applied;
        c.applied = text.chars().count() as u32;
        let out = Feed::Cmd(Command::CompleteWord { back, text });
        self.record(&out, Mode::Insert);
        out
    }

    fn action(&mut self, cmd: Command) -> Feed {
        self.reset();
        Feed::Cmd(cmd)
    }

    /// Dispatch a PURE insert-entry (`i`/`a`/`I`/`A`/`o`/`O`), capturing the leading count for
    /// count-on-insert repetition (VIM-CNT-INS) BEFORE [`action`](Self::action) resets the axes. The
    /// captured count is stashed in `pending_insert_count`; [`record`](Self::record) folds it into the
    /// [`ChangeIntent`] this entry opens, and the terminating `<Esc>` replays the typed text that many
    /// times. Change-family entries (`c`/`s`/…) do NOT come through here, so their count never repeats text.
    fn insert_entry(&mut self, cmd: Command) -> Feed {
        self.pending_insert_count = self.normal.count.max(1);
        self.action(cmd)
    }

    /// A named-mark key after `` ` ``/`'`: with an operator pending it OPERATES to the mark
    /// (`` d`a ``/`d'a`, `` g~`a ``, `` >`a ``, `` =`a ``); otherwise it JUMPS (`` `a ``/`'a`). `linewise`
    /// is the `'` form (honoured by delete/change/yank/case; shift/reindent are always linewise). `gq`/`gw`
    /// (Format) to a mark is not modeled — it falls back to the bare jump.
    fn mark_op(&mut self, name: char, linewise: bool) -> Feed {
        let op = self.normal.op.and_then(|p| match p.op {
            Op::Delete => Some(MarkOp::Delete),
            Op::Change => Some(MarkOp::Change),
            Op::Yank => Some(MarkOp::Yank),
            Op::CaseLower => Some(MarkOp::Case(WordCase::Downcase)),
            Op::CaseUpper => Some(MarkOp::Case(WordCase::Upcase)),
            Op::CaseToggle => Some(MarkOp::Case(WordCase::Toggle)),
            Op::Rot13 => Some(MarkOp::Case(WordCase::Rot13)),
            Op::ShiftRight => Some(MarkOp::Shift { left: false }),
            Op::ShiftLeft => Some(MarkOp::Shift { left: true }),
            Op::Reindent => Some(MarkOp::Reindent),
            // `gq`/`gw` (Format) and `!` (Filter) to a mark are not modeled; fall through to a bare jump.
            Op::Format | Op::FormatKeep | Op::Filter => None,
        });
        let cmd = match op {
            Some(op) => Command::OpToMark { op, name, linewise },
            None if linewise => Command::GotoNamedMarkLine(name),
            None => Command::GotoNamedMark(name),
        };
        self.action(cmd)
    }

    /// Emit `gn`/`gN` — the search-match text object. Reads the last search pattern and folds in any pending
    /// operator (`dgn`/`cgn`/`ygn`); the bare form ([`SearchOp::Move`]) selects the match in Visual. With no
    /// prior search there is nothing to match, so the pending construct aborts (leaking no state). Emitted as
    /// a single [`Command::SearchObject`] with the pattern baked in, so `.` can replay `cgn`.
    fn search_object(&mut self, backward: bool) -> Feed {
        let Some((pattern, _, _)) = self.last_search.clone() else {
            // No prior search — nothing to match; abort the pending construct, leaking no state.
            self.reset();
            return Feed::Ignored;
        };
        let op = match self.normal.op {
            Some(OpPending { op: Op::Delete, .. }) => SearchOp::Delete,
            Some(OpPending { op: Op::Change, .. }) => SearchOp::Change,
            Some(OpPending { op: Op::Yank, .. }) => SearchOp::Yank,
            // No operator (or a non-d/c/y one that gn does not accept): the bare, Visual-selecting form.
            _ => SearchOp::Move,
        };
        let count = self.normal.op.map_or(1, |p| p.count.max(1)) * self.mcount();
        self.reset();
        Feed::Cmd(Command::SearchObject {
            op,
            count,
            pattern,
            backward,
        })
    }

    /// Arm a case operator (`gu`/`gU`/`g~`) so the next motion recases its span. No doubling detection
    /// here — the linewise `guu`/`gUU`/`g~~` form is handled where the second key is dispatched.
    fn arm_case_op(&mut self, op: Op) -> Feed {
        self.normal.op = Some(OpPending {
            op,
            count: self.mcount(),
        });
        self.normal.count = 0;
        Feed::Pending
    }

    /// Emit the LINEWISE case command for a doubled case operator (`guu`/`gUU`/`g~~`) — recase the current
    /// `count` lines. Called only with a case operator armed.
    fn case_linewise(&mut self) -> Feed {
        let cmd = match self.normal.op {
            Some(OpPending { op, count }) => {
                let case = op.case().expect("a case operator is armed");
                Command::CaseMotion {
                    count: count.max(1) * self.mcount(),
                    motion: Motion::Line,
                    case,
                }
            }
            None => unreachable!("case_linewise called without an armed case operator"),
        };
        self.reset();
        Feed::Cmd(cmd)
    }

    /// Arm an operator, or emit its linewise form when doubled (`dd`/`cc`/`yy`). `linewise` builds the
    /// linewise command from the operator's count.
    fn operator(&mut self, op: Op, linewise: fn(u32, Motion) -> Command) -> Feed {
        if let Some(OpPending { op: armed, count }) = self.normal.op {
            if armed == op {
                let n = count.max(1);
                self.reset();
                return Feed::Cmd(linewise(n, Motion::Line));
            }
        }
        self.normal.op = Some(OpPending {
            op,
            count: self.mcount(),
        });
        self.normal.count = 0;
        Feed::Pending
    }

    /// Feed one key given the current mode. Resolves the key into an outcome, then folds that outcome into
    /// the dot-repeat record (so `.` can later replay the last change). The two steps are split so the
    /// resolution grammar stays untouched by the recording concern.
    /// Install/replace one Lang-Arg mapping (`:lmap {lhs} {rhs}`, F-027). MVP is single-char → single-char.
    pub fn set_lang_mapping(&mut self, lhs: char, rhs: char) {
        self.lang_map.insert(lhs, rhs);
    }

    /// Remove one Lang-Arg mapping (`:lunmap {lhs}`). A no-op if it was not mapped.
    pub fn clear_lang_mapping(&mut self, lhs: char) {
        self.lang_map.remove(&lhs);
    }

    /// Whether `mode` (plus the engine's live state) is a Lang-Arg-eligible context: the Command-line or
    /// Insert namespace, or a command reading a single character (`f`/`t`/`F`/`T`/`r`). Everything else —
    /// Normal, Visual, Operator-pending, Replace — is inert, which is the whole point of a SEPARATE Lang
    /// namespace (F-027 acceptance #2: "and to nothing else"): operators and motions are never translated.
    fn lang_eligible(&self, mode: Mode) -> bool {
        // Command-line namespace (typing on the `:`/`/` line).
        if self.cmdline.is_some() {
            return true;
        }
        // A command reading a single character as its argument: `f`/`t`/`F`/`T` (find target) or `r`
        // (replace). Vim's Lang-Arg translates that argument regardless of how the command was reached.
        if matches!(
            self.normal.awaiting,
            Awaiting::FindTarget { .. } | Awaiting::ReplaceChar
        ) {
            return true;
        }
        // Insert namespace — but NOT mid-`CTRL-G` prefix (its second key is a command selector, not
        // text), NOT mid-`CTRL-K` digraph prefix (its two keys are literal digraph-code selectors, not
        // text), NOT mid-`CTRL-V` literal entry (its form selector / code digits / literal key are raw,
        // never mapped), and NOT while an `i_CTRL-O` one-shot has borrowed the Normal grammar (that key
        // is a Normal command, not inserted text).
        mode == Mode::Insert
            && !self.insert.ctrl_g
            && self.insert.digraph.is_none()
            && self.insert.literal.is_none()
            && !self.in_one_shot()
    }

    /// The Lang-Arg TRANSLATION STAGE (F-027, D-048 / RFC-0013). Rewrites a decoded key through the
    /// active language map BEFORE any dispatch, in the Lang-Arg contexts only, applying AT MOST ONE
    /// substitution: the mapped key is returned and dispatched literally, never fed back through this
    /// stage — so a cyclic map (`a→b`, `b→a`) cannot loop, resolution stays TOTAL, and the work stays
    /// BOUNDED (INV-FAIL-BOUNDED). The stage lives ABOVE the layer stack (D-045 `resolve` is untouched):
    /// it is a preprocessor that always yields a concrete key, not a resolution layer that yields.
    ///
    /// Terminal-side IME composition is a DIFFERENT, disjoint mechanism (acceptance #3): the terminal
    /// composes keystrokes into a finished CHARACTER and delivers it as TEXT on the paste/IME path, which
    /// never reaches `feed` as a single decoded `KeyEvent`. So this stage only ever sees a bare printable
    /// key with no CTRL/ALT modifier, and a given unit of input is translated by AT MOST ONE of {terminal
    /// IME, lmap} — never both.
    fn translate_lang(&self, key: KeyEvent, mode: Mode) -> KeyEvent {
        if !self.lang_active || self.lang_map.is_empty() {
            return key;
        }
        // Only a bare printable key is a candidate; a modified key (CTRL-*/ALT-*) is a command, and
        // composed IME text never arrives here at all.
        let plain = key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT;
        let KeyCode::Char(c) = key.code else {
            return key;
        };
        if !plain || !self.lang_eligible(mode) {
            return key;
        }
        match self.lang_map.get(&c) {
            Some(&mapped) => KeyEvent {
                code: KeyCode::Char(mapped),
                ..key
            },
            None => key,
        }
    }

    pub fn feed(&mut self, key: KeyEvent, mode: Mode) -> Feed {
        // Lang-Arg translation stage (F-027 / D-048): rewrite the key through the active language map
        // before ANY dispatch, in the three Lang-Arg contexts only. One substitution, then literal —
        // the translated key flows through normal dispatch and never re-enters this stage.
        let key = self.translate_lang(key, mode);
        // Emacs is NON-MODAL: no Normal/Insert grammar, no operator-pending, no dot-repeat recorder — a
        // key is either a `C-`/`M-` command or literal text. So it takes its own dispatch, never the Vim
        // modal path below (F-012 / RFC-0014). Native is NOT here on purpose: its text layer IS the Vim
        // modal grammar (F-013 NAT-1), so it falls through to the modal path and only its additive layers
        // (leader/which-key, transient maps) — landing in later slices — will branch above it.
        if self.input_profile == InputProfile::Emacs {
            return self.feed_emacs(key);
        }
        // The command-line window owns the keystream while open (`:help cmdwin`, `q:`/`q/`/`q?`): it is a
        // navigation overlay, not a dot-repeatable change, so it bypasses the recorder like the cmdline.
        if self.cmdwin.is_some() {
            return self.feed_cmdwin(key);
        }
        // The command-line namespace owns the keystream while open (F-026); its typing is not a
        // dot-repeatable change, so it bypasses the recorder.
        if self.cmdline.is_some() {
            return self.feed_cmdline(key);
        }
        let out = self.feed_impl(key, mode);
        self.record(&out, mode);
        // Count-on-insert (VIM-CNT-INS): a count-prefixed insert session that just closed on `<Esc>`
        // stages its extra repeats + trailing `EnterNormal` here — return them as one `Feed::Replay` in
        // place of the bare `EnterNormal` so the whole `3ihello` collapses into a single undo group.
        if let Some(tail) = self.pending_insert_replay.take() {
            return Feed::Replay(tail);
        }
        out
    }

    /// The Emacs profile's dispatch (F-012, minimal slice). Non-modal: the global-map's `C-` motions
    /// resolve to commands; an unmodified printable key inserts literally. A `C-u`/digit prefix argument
    /// (D-049) accumulates ahead of the command it modifies. This is the seam — the nine-tier stack, the
    /// `C-x` prefix maps, the kill ring and the mark ring layer on from here (RFC-0014). Motions work while
    /// the buffer stays editable because a `Move*` command keeps the current mode and only moves the cursor,
    /// so "move + insert in one state" needs no new mode.
    fn feed_emacs(&mut self, key: KeyEvent) -> Feed {
        // The `M-x` minibuffer owns the keystream while open (F-026 command-line namespace), reused verbatim
        // from the Vim `:`-line handler — its `<CR>` resolves the command NAME (the `cl.mx` branch).
        if self.cmdline.is_some() {
            return self.feed_cmdline(key);
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        // A pending prefix key owns the NEXT keystroke: resolve it in that prefix's map before any global
        // dispatch. `C-g` (keyboard-quit) or any unbound key cancels the prefix (Emacs beeps). The prefix
        // is always cleared here, so a single stray key can never leave the engine wedged in a prefix.
        if let Some(prefix) = self.emacs_prefix.take() {
            return self.feed_emacs_prefix(prefix, key, ctrl);
        }
        // `C-u` (universal argument): seed the prefix argument, or multiply an in-progress one by four.
        // It never completes a command — it always leaves the argument pending for the next key.
        if ctrl && key.code == KeyCode::Char('u') {
            self.emacs_arg = Some(match self.emacs_arg {
                Some(arg) => arg.times_four(),
                None => EmacsArg::ctrl_u(),
            });
            return Feed::Pending;
        }
        // While an argument is being read, an unmodified digit extends it (an explicit numeric count) rather
        // than self-inserting. Only when an argument is already pending — a bare digit is ordinary text.
        if self.emacs_arg.is_some() && !ctrl {
            if let KeyCode::Char(d @ '0'..='9') = key.code {
                let arg = self.emacs_arg.unwrap_or_else(EmacsArg::ctrl_u);
                self.emacs_arg = Some(arg.push_digit(d as u32 - '0' as u32));
                return Feed::Pending;
            }
        }
        // Any other key completes the argument: take the accumulated count (default 1) and resolve the key
        // through the nine-tier stack (F-012 / D-045). The count is folded per the bound key's policy.
        let count = self.emacs_arg.take().map(EmacsArg::count).unwrap_or(1);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        // Resolve, then drop the borrow of `self.emacs` before mutating engine state (the prefix map).
        let step = match self.emacs.map.resolve(&EmacsKey::of(key)) {
            Resolved::Bound { value, .. } => Step::Bound(value.clone()),
            // global-map's `self-insert-command`: an UNMODIFIED printable key inserts. A `C-`/`M-` key that
            // reached here is unbound, not text, so it stays inert.
            Resolved::Unmatched {
                policy: UnmatchedKey::Insert,
                ..
            } if !ctrl && !alt => match key.code {
                KeyCode::Char(c) => Step::SelfInsert(c),
                _ => Step::Ignore,
            },
            _ => Step::Ignore,
        };
        match step {
            Step::Bound(EmacsBinding::Prefix(p)) => {
                // Enter the prefix map: the next key resolves there. Any pending argument is dropped in this
                // slice (arg-passthrough to a prefixed command is a follow-up).
                self.emacs_prefix = Some(p);
                Feed::Pending
            }
            Step::Bound(EmacsBinding::Minibuffer) => {
                // M-x: open the minibuffer; subsequent keys route through `feed_cmdline` until `<CR>`.
                self.open_minibuffer();
                Feed::Pending
            }
            Step::Bound(binding) => fold_emacs_count(&binding, count),
            Step::SelfInsert(c) => emacs_repeat(Command::InsertChar(c), count),
            Step::Ignore => Feed::Ignored,
        }
    }

    /// Resolve the second key of an Emacs prefix sequence (F-012). Only the `C-x` map exists in this slice:
    /// `C-x C-s` saves, `C-x C-c` quits, `C-x u` undoes, `C-x C-x` exchanges point and mark. An unbound key
    /// (including `C-g`) cancels the prefix and is inert — the prefix was already cleared by the caller, so
    /// the engine is never left wedged.
    fn feed_emacs_prefix(&mut self, prefix: char, key: KeyEvent, ctrl: bool) -> Feed {
        if prefix == 'x' {
            let cmd = match key.code {
                KeyCode::Char('s') if ctrl => Command::Save, // C-x C-s — save-buffer
                KeyCode::Char('c') if ctrl => Command::Quit, // C-x C-c — save-buffers-kill-terminal
                KeyCode::Char('u') if !ctrl => Command::Undo, // C-x u — undo
                KeyCode::Char('x') if ctrl => Command::ExchangePointMark, // C-x C-x — exchange point/mark
                KeyCode::Char('u') if ctrl => Command::EmacsCaseRegion {
                    case: WordCase::Upcase, // C-x C-u — upcase-region
                },
                KeyCode::Char('l') if ctrl => Command::EmacsCaseRegion {
                    case: WordCase::Downcase, // C-x C-l — downcase-region
                },
                _ => return Feed::Ignored,
            };
            return Feed::Cmd(cmd);
        }
        Feed::Ignored
    }

    /// Fold a just-produced outcome into the dot-repeat record. In Insert mode, extend the in-flight change
    /// until `<Esc>` closes it; in Normal/Visual, an insert-entering command opens a recording, a
    /// self-contained edit becomes the record outright, and anything else leaves the record intact.
    /// `Pending`/`Ignored`/`Replay`/`Open*` never touch it — which is what makes `..` repeat one change.
    fn record(&mut self, out: &Feed, mode: Mode) {
        let Feed::Cmd(cmd) = out else {
            return;
        };
        if mode == Mode::Insert {
            if let Some(rec) = self.recording.as_mut() {
                rec.insert.push(cmd.clone());
                // The `<Esc>` that leaves Insert (recorded here so replay leaves Insert too) closes the change.
                if *cmd == Command::EnterNormal {
                    let closed = self.recording.take();
                    // Count-on-insert (VIM-CNT-INS): if a count preceded the entry, stage the extra repeats
                    // so `feed` returns them as ONE undo-grouped `Feed::Replay` in place of this `<Esc>`.
                    if let Some(rec) = &closed {
                        self.pending_insert_replay = rec.count_replay_tail();
                        // Remember the typed text for `i_CTRL-A` / `i_CTRL-@` (Vim's `".` register). Unlike
                        // `last_change`, this survives a later non-insert change (`x`, `dw`) — verified vs
                        // nvim — so it is captured here on close and never touched by an Immediate change.
                        self.last_inserted = rec.insert_body().to_vec();
                    }
                    self.last_change = closed;
                }
            }
            return;
        }
        // `"x` selects the register the NEXT recorded change should carry — remembered, not itself a change.
        if let Command::SetRegister(name) = cmd {
            self.pending_record_register = *name;
            return;
        }
        match change_kind(cmd) {
            ChangeKind::InsertEntering => {
                self.recording = Some(ChangeIntent {
                    lead: cmd.clone(),
                    insert: Vec::new(),
                    register: self.pending_record_register.take(),
                    // The count a pure insert-entry captured (`3i`); `0` (→ 1) for a count-less or
                    // change-family entry, so only `i`/`a`/`I`/`A`/`o`/`O` ever repeat their text.
                    entry_count: std::mem::take(&mut self.pending_insert_count).max(1),
                });
            }
            ChangeKind::Immediate => {
                self.recording = None;
                self.last_change = Some(ChangeIntent {
                    lead: cmd.clone(),
                    insert: Vec::new(),
                    register: self.pending_record_register.take(),
                    entry_count: 1,
                });
            }
            // A non-change (motion, mode switch, yank) forgets a dangling register selection.
            ChangeKind::NotAChange => self.pending_record_register = None,
        }
    }

    /// Feed one key given the current mode.
    /// The Insert namespace (mode is Insert, no one-shot in flight). Two multi-key sequences resolve
    /// before the layer — `CTRL-G u` (undo-break), `i_CTRL-^` (Lang-Arg toggle), `CTRL-O` (one-shot
    /// Normal), `CTRL-G` (prefix) — then the Insert layer binds, else the `open/insert` policy applies.
    /// `i_CTRL-A` / `i_CTRL-@` (`:help i_CTRL-A`): re-insert the text of the MOST RECENT insert session
    /// (Vim's `".` register) at the caret. `leave` selects the `CTRL-@` variant, which is "`CTRL-A` then
    /// `<Esc>`" — it additionally leaves Insert, and does so EVEN when there is no previous insert
    /// (verified vs nvim). With no previous insert the stored body is empty, so plain `CTRL-A` is a clean
    /// no-op (nvim rings the bell — "E29: No inserted text yet").
    ///
    /// The body commands are replayed VERBATIM: replaying them (backspaces and all) reproduces the
    /// RESULTING text of that session, exactly as nvim's keystroke replay does. They are also folded into
    /// any in-flight recording so the re-inserted text rolls into the SESSION's `".` and dot-repeat
    /// (matches nvim). The multi-command [`Feed::Replay`] channel bypasses the recorder, so this fold is
    /// done by hand — the same pattern [`feed_ctrl_v`](Self::feed_ctrl_v) uses.
    fn insert_prev_text(&mut self, leave: bool) -> Feed {
        self.reset();
        if self.last_inserted.is_empty() && !leave {
            // Nothing to insert and no mode change to make: a pure no-op.
            return Feed::Ignored;
        }
        let mut cmds = self.last_inserted.clone();
        if leave {
            cmds.push(Command::EnterNormal);
        }
        // Fold each replayed command into the in-flight recording exactly as the outer `feed` would for a
        // sequence of `Cmd`s — recording the trailing `EnterNormal` (the `CTRL-@` case) also closes the
        // session into `last_change` / `last_inserted`, so the next `".` reflects the re-inserted text.
        for cmd in &cmds {
            self.record(&Feed::Cmd(cmd.clone()), Mode::Insert);
        }
        Feed::Replay(cmds)
    }

    fn feed_insert(&mut self, key: KeyEvent) -> Feed {
        // Any key reaching `feed_insert` is NOT a completion-cycle key (`CTRL-N`/`CTRL-P` are intercepted by
        // the frontend before `feed`), so it ACCEPTS the current candidate and ends the cycle (Vim). The
        // accepted text already sits in the buffer; dropping the state is all that is needed.
        self.insert.completion = None;
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        // `CTRL-R` prefix: consume the second key as the register NAME and insert its contents at the caret.
        // Accepts the same names the paste path reads (`"`, `0`–`9`, `-`, `a`–`z`/`A`–`Z`); any other key
        // aborts without inserting. Checked before the layer so the register key never reaches text insertion.
        if self.insert.ctrl_r {
            self.insert.ctrl_r = false;
            // A CONTROL-modified second key is NOT a register name. Vim's `i_CTRL-R_CTRL-W`/`_CTRL-A` insert
            // NOTHING (the word-under-cursor inserts are COMMAND-LINE mode only — `c_CTRL-R_CTRL-W`), and the
            // `C-r C-r`/`C-o`/`C-p` insert-literally variants are not implemented. Treat every ctrl-modified
            // selector as a clean no-op rather than mis-reading the char as a register (previously `C-r C-w`
            // wrongly inserted register `w`, `C-r C-a` register `a`). Verified against nvim v0.12.4.
            if key.modifiers.contains(KeyModifiers::CONTROL) {
                self.reset();
                return Feed::Ignored;
            }
            // `i_CTRL-R=` — the expression register (`:help i_CTRL-R`, the MOST common use): open the
            // expression prompt; on `<CR>` the evaluated result is spliced at the caret. Handled before the
            // stored-register names below.
            if key.code == KeyCode::Char('=') {
                self.reset();
                self.open_expr_prompt(ExprTarget::Insert);
                return Feed::Pending;
            }
            return match key.code {
                KeyCode::Char(c)
                    if c == '"'
                        || c == '-'
                        || c == '+'
                        || c == '*'
                        // The read-only special registers `"/ ": ". "%` (`:help i_CTRL-R`): `C-r /`
                        // inserts the last search pattern, `C-r :` the last Ex line, `C-r .` the last
                        // inserted text, `C-r %` the file name. Resolved by the frontend's `set_special`.
                        || c == '/'
                        || c == ':'
                        || c == '.'
                        || c == '%'
                        || c.is_ascii_alphanumeric() =>
                {
                    self.action(Command::InsertRegister(c))
                }
                _ => {
                    self.reset();
                    Feed::Ignored
                }
            };
        }
        // `CTRL-G` prefix: consume the second key. `u` (or `U`) breaks the undo group; anything else
        // aborts the prefix without inserting (Vim beeps). Checked before the layer so the printable
        // path never sees the prefixed key.
        if self.insert.ctrl_g {
            self.insert.ctrl_g = false;
            return match key.code {
                // `action` clears the transient axes (like every other completed key), so no partial
                // Normal state can survive an Insert key.
                KeyCode::Char('u') | KeyCode::Char('U') => self.action(Command::BreakUndo),
                _ => {
                    self.reset();
                    Feed::Ignored
                }
            };
        }
        // `i_CTRL-K` digraph prefix: once armed, the next TWO printable keys are the digraph code
        // (`:help i_CTRL-K`). Collected here, before the layer, so the code chars never reach text
        // insertion. A non-`Char` key (Esc, arrows, …) mid-sequence ABORTS cleanly — the pending state is
        // dropped and the key is ignored, staying in Insert (a minor divergence: Vim's mid-digraph <Esc>
        // also leaves Insert; here it only cancels the digraph). On the second char the pair is looked up;
        // an unknown pair falls back to inserting the SECOND char literally, matching Vim.
        if let Some(pending) = self.insert.digraph {
            let KeyCode::Char(c) = key.code else {
                self.insert.digraph = None;
                self.reset();
                return Feed::Ignored;
            };
            match pending {
                DigraphPending::First => {
                    self.insert.digraph = Some(DigraphPending::Second(c));
                    return Feed::Pending;
                }
                DigraphPending::Second(c1) => {
                    self.insert.digraph = None;
                    let ch = digraph(c1, c).unwrap_or(c);
                    return self.action(Command::InsertChar(ch));
                }
            }
        }
        // `i_CTRL-V` literal / numeric-code entry: once armed, drive the collector (`:help i_CTRL-V`).
        // Checked before the layer so the form selector / code digits / literal key never reach normal
        // text insertion. `take()` clears the state; the collector re-arms it while still gathering.
        if let Some(entry) = self.insert.literal.take() {
            return self.feed_ctrl_v(entry, key);
        }
        // `i_CTRL-^` toggles the language map (Lang-Arg / lmap) on or off within Insert (F-027 / D-048).
        // MVP flips one boolean; the per-context iminsert/imsearch model is a follow-up. Checked before
        // the printable path so `^`/`6` under CTRL never reach text insertion.
        if ctrl && matches!(key.code, KeyCode::Char('^') | KeyCode::Char('6')) {
            self.reset();
            self.lang_active = !self.lang_active;
            return Feed::Pending;
        }
        if ctrl && key.code == KeyCode::Char('o') {
            // Push a one-shot activation whose RETURN ADDRESS is Insert (KL-OBL-5): the NEXT keys resolve
            // through the Normal grammar; on completion `reset()` pops the address and Insert routing
            // resumes. Core mode stays Insert throughout. Reset first so the one-shot begins from a clean
            // count/operator/awaiting state (the pop is a no-op — the stack is empty here).
            self.reset();
            self.activations.push(Suspended { resume: Ns::Insert });
            return Feed::Pending;
        }
        if ctrl && key.code == KeyCode::Char('g') {
            self.reset();
            self.insert.ctrl_g = true;
            return Feed::Pending;
        }
        // `i_CTRL-R` — arm the register-insert prefix; the next key names the register.
        if ctrl && key.code == KeyCode::Char('r') {
            self.reset();
            self.insert.ctrl_r = true;
            return Feed::Pending;
        }
        // `i_CTRL-K` — arm the digraph prefix; the next TWO printable keys select the digraph.
        if ctrl && key.code == KeyCode::Char('k') {
            self.reset();
            self.insert.digraph = Some(DigraphPending::First);
            return Feed::Pending;
        }
        // `i_CTRL-V` — arm literal / numeric char entry; the next key(s) select the form and code
        // (Normal-mode `CTRL-V` is blockwise Visual — a separate path; this only fires in Insert).
        if ctrl && key.code == KeyCode::Char('v') {
            self.reset();
            self.insert.literal = Some(LiteralEntry::AwaitFirst);
            return Feed::Pending;
        }
        // `i_CTRL-W` / `i_CTRL-U` — delete the word before the caret / everything before it on the line.
        if ctrl && key.code == KeyCode::Char('w') {
            return self.action(Command::InsertDeleteWordBack);
        }
        if ctrl && key.code == KeyCode::Char('u') {
            return self.action(Command::InsertDeleteToLineStart);
        }
        // `i_CTRL-T` / `i_CTRL-D` — indent / dedent the current line by one shiftwidth.
        if ctrl && key.code == KeyCode::Char('t') {
            return self.action(Command::InsertIndent);
        }
        if ctrl && key.code == KeyCode::Char('d') {
            return self.action(Command::InsertDedent);
        }
        // `i_CTRL-A` — re-insert the text of the MOST RECENT insert session at the caret (Vim's `".`
        // register). `i_CTRL-@` does the same and then leaves Insert (`:help i_CTRL-@`). Some terminals
        // deliver `CTRL-@` as `Ctrl+Space`; both are accepted here (Insert `Ctrl+Space` is otherwise
        // unbound in the Vim/Native profiles).
        if ctrl && key.code == KeyCode::Char('a') {
            return self.insert_prev_text(false);
        }
        if ctrl && matches!(key.code, KeyCode::Char('@') | KeyCode::Char(' ')) {
            return self.insert_prev_text(true);
        }
        // `<Tab>` inserts whitespace to the next tabstop (spaces under `expandtab`, else a hard tab). Without
        // this, Tab falls through to the Insert unmatched policy, which only emits for `Char` keys — so a raw
        // `KeyCode::Tab` would silently do nothing.
        if key.code == KeyCode::Tab {
            return self.action(Command::InsertTab);
        }
        if let Resolved::Bound { value, .. } = self.profile.stack(Ns::Insert).resolve(&key.code) {
            let cmd = value.clone();
            self.reset();
            return Feed::Cmd(cmd);
        }
        self.unmatched(Ns::Insert, key)
    }

    /// `i_CTRL-V` literal / numeric-code entry (`:help i_CTRL-V`). Drives the collector armed by
    /// `CTRL-V` in Insert. The first key picks a numeric form (decimal `{ddd}`, octal `o{ooo}`, hex
    /// `x{hh}`, BMP unicode `u{hhhh}`, full unicode `U{hhhhhhhh}`) or is inserted literally (a raw
    /// control key, `<Tab>`/`<CR>`/`<Esc>`, or any non-form char). Subsequent keys are the code digits.
    /// Collection resolves when the per-form digit cap is reached OR a non-matching key terminates it
    /// early — in which case the resolved char is emitted and the terminating key is processed as normal
    /// Insert input (re-dispatched here), matching Vim. Insert mode is implicit (the only caller).
    fn feed_ctrl_v(&mut self, entry: LiteralEntry, key: KeyEvent) -> Feed {
        match entry {
            LiteralEntry::AwaitFirst => self.ctrl_v_first(key),
            LiteralEntry::Collecting {
                base,
                value,
                remaining,
                count,
            } => self.ctrl_v_collect(base, value, remaining, count, key),
        }
    }

    /// The FIRST key after `CTRL-V`: arm a numeric form, or insert the key literally.
    fn ctrl_v_first(&mut self, key: KeyEvent) -> Feed {
        if !key.modifiers.contains(KeyModifiers::CONTROL) {
            if let KeyCode::Char(c) = key.code {
                if let Some(d) = c.to_digit(10) {
                    // Decimal: this char is the first of up to three digits.
                    self.insert.literal = Some(LiteralEntry::Collecting {
                        base: LiteralBase::Dec,
                        value: d,
                        remaining: 2,
                        count: 1,
                    });
                    return Feed::Pending;
                }
                // `o`/`O` octal (3), `x`/`X` hex byte (2), `u` BMP (4), `U` full Unicode (8).
                let form = match c {
                    'o' | 'O' => Some((LiteralBase::Oct, 3)),
                    'x' | 'X' => Some((LiteralBase::Hex, 2)),
                    'u' => Some((LiteralBase::Hex, 4)),
                    'U' => Some((LiteralBase::Hex, 8)),
                    _ => None,
                };
                if let Some((base, budget)) = form {
                    self.insert.literal = Some(LiteralEntry::Collecting {
                        base,
                        value: 0,
                        remaining: budget,
                        count: 0,
                    });
                    return Feed::Pending;
                }
            }
        }
        // Not a form starter: insert the key literally.
        self.insert_literal_key(key)
    }

    /// A key during numeric collection: extend the code if it is a valid digit for `base`, else terminate.
    fn ctrl_v_collect(
        &mut self,
        base: LiteralBase,
        value: u32,
        remaining: u8,
        count: u8,
        key: KeyEvent,
    ) -> Feed {
        let digit = match key.code {
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                c.to_digit(base.radix())
            }
            _ => None,
        };
        if let Some(d) = digit {
            let value = value * base.radix() + d;
            let remaining = remaining - 1;
            let count = count + 1;
            if remaining == 0 {
                // Digit cap reached: resolve now; there is no terminating key to re-process.
                return self.resolve_literal(base, value);
            }
            self.insert.literal = Some(LiteralEntry::Collecting {
                base,
                value,
                remaining,
                count,
            });
            return Feed::Pending;
        }
        // A non-digit terminates collection.
        if count == 0 {
            // No digits were collected (only reachable for the o/x/u/U prefixes): the terminating key is
            // inserted literally and no code char is produced (Vim: `CTRL-V x z` -> `z`, `CTRL-V o 8` -> `8`).
            return self.insert_literal_key(key);
        }
        // >=1 digit: resolve the code, then process the terminating key as normal Insert input.
        let resolved = self.resolve_char(base, value);
        let term = self.feed_impl(key, Mode::Insert);
        match resolved {
            // Invalid code point (a lone surrogate via `u`/`U`, say): nothing to prepend — just the term.
            None => term,
            Some(c) => match term {
                Feed::Cmd(tc) => {
                    // Emit BOTH the resolved char and the terminator's command. `Replay` is the only
                    // multi-command channel and it bypasses the recorder, so fold both into the dot-repeat
                    // record by hand (exactly what the outer `feed` would do for two consecutive `Cmd`s).
                    self.record(&Feed::Cmd(Command::InsertChar(c)), Mode::Insert);
                    self.record(&Feed::Cmd(tc.clone()), Mode::Insert);
                    Feed::Replay(vec![Command::InsertChar(c), tc])
                }
                // The terminator armed a prefix / was ignored: only the resolved char applies now. Any
                // Insert prefix it just armed survives `action`'s reset (which clears only Normal state).
                _ => self.action(Command::InsertChar(c)),
            },
        }
    }

    /// Insert `key` literally (`CTRL-V`'s literal path): a raw control key becomes its control byte,
    /// `<Tab>`/`<CR>`/`<Esc>` their code points, any other char itself. An exotic key (arrows, F-keys)
    /// aborts cleanly, staying in Insert.
    fn insert_literal_key(&mut self, key: KeyEvent) -> Feed {
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            if let KeyCode::Char(c) = key.code {
                if c.is_ascii() {
                    // `CTRL-<c>` -> its control byte (`CTRL-V` -> 0x16, `CTRL-A` -> 0x01, ...).
                    return self.action(Command::InsertChar(((c as u8) & 0x1f) as char));
                }
            }
            self.reset();
            return Feed::Ignored;
        }
        match key.code {
            KeyCode::Char(c) => self.action(Command::InsertChar(c)),
            KeyCode::Tab => self.action(Command::InsertChar('\t')),
            KeyCode::Enter => self.action(Command::InsertChar('\r')),
            KeyCode::Esc => self.action(Command::InsertChar('\u{1b}')),
            _ => {
                self.reset();
                Feed::Ignored
            }
        }
    }

    /// Resolve an accumulated numeric code to its `InsertChar` command (the cap-reached path).
    fn resolve_literal(&mut self, base: LiteralBase, value: u32) -> Feed {
        match self.resolve_char(base, value) {
            Some(c) => self.action(Command::InsertChar(c)),
            None => {
                self.reset();
                Feed::Ignored
            }
        }
    }

    /// Map an accumulated numeric code to a `char`. Decimal/octal clamp to a single byte (255, Vim's cap);
    /// hex/unicode are validated by `char::from_u32` — a lone surrogate or an out-of-range code point
    /// yields `None`, which the caller drops. (A documented divergence: Vim stores such raw bytes as
    /// invalid UTF-8, which ruse's UTF-8 text buffer cannot represent via `InsertChar`. Byte values
    /// 0-255 map to U+0000..=U+00FF and are inserted as valid UTF-8, matching Neovim's `C-v 200` -> È.)
    fn resolve_char(&self, base: LiteralBase, value: u32) -> Option<char> {
        let v = match base {
            LiteralBase::Dec | LiteralBase::Oct => value.min(255),
            LiteralBase::Hex => value,
        };
        char::from_u32(v)
    }

    /// Replace (`R`) / Virtual Replace (`gR`): the `open/overwrite` namespace. Bindings (Esc/BS/CR)
    /// resolve through the Replace LAYER; an unmatched printable key hits the layer's declared overwrite
    /// policy (KL-OBL-2), applied as the mode-appropriate overwrite command (tab-aware in Virtual Replace).
    fn feed_replace(&mut self, key: KeyEvent, mode: Mode) -> Feed {
        if let Resolved::Bound { value, .. } = self.profile.stack(Ns::Replace).resolve(&key.code) {
            let cmd = value.clone();
            return self.action(cmd);
        }
        // open/overwrite: a printable key overwrites; non-printable does nothing (NOT closed/ignore).
        debug_assert!(
            matches!(
                self.profile.stack(Ns::Replace).resolve(&key.code),
                Resolved::Unmatched {
                    policy: UnmatchedKey::Overwrite,
                    ..
                }
            ),
            "the Replace namespace must declare open/overwrite"
        );
        self.reset();
        match key.code {
            KeyCode::Char(c) if mode == Mode::VirtualReplace => {
                Feed::Cmd(Command::VirtualReplaceType(c))
            }
            KeyCode::Char(c) => Feed::Cmd(Command::ReplaceType(c)),
            _ => Feed::Ignored,
        }
    }

    /// The base Normal-family dispatch, reached only with `awaiting == Nothing` (the tier above
    /// returned for the pending cases). Shared char-search initiators, then the count/operator/motion
    /// grammar and the mode-specific keys.
    fn feed_base(&mut self, key: KeyEvent, mode: Mode) -> Feed {
        // `CTRL-G` toggles Visual<->Select over the SAME selection (Vim's documented behaviour). Handled
        // here, before the shared `g` initiator below, so it is never mistaken for the start of `gg`. In
        // Normal it is Vim's file-info command (name / [Modified] / line count / cursor percent), resolved
        // + status-surfaced by the frontend. In any other non-selection mode nothing is bound (inert, NOT
        // the start of `gg`).
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('g') {
            return match mode {
                Mode::Visual { kind } => self.action(Command::EnterSelect { kind }),
                Mode::Select { kind } => self.action(Command::EnterVisual { kind }),
                Mode::Normal => self.action(Command::FileInfo),
                _ => {
                    self.reset();
                    Feed::Ignored
                }
            };
        }
        // --- Shared initiators (char-search + `;`/`,`): work in Normal and Visual, preserving the operator
        // axis (so `dfx` / `d;` work). Reachable only with `awaiting == Nothing` — the tier above already
        // returned for the pending cases — so a text object in flight can never be hijacked by `f`/`t`. ---
        match key.code {
            KeyCode::Char('f') => {
                self.normal.awaiting = Awaiting::FindTarget {
                    forward: true,
                    till: false,
                };
                return Feed::Pending;
            }
            KeyCode::Char('F') => {
                self.normal.awaiting = Awaiting::FindTarget {
                    forward: false,
                    till: false,
                };
                return Feed::Pending;
            }
            KeyCode::Char('t') => {
                self.normal.awaiting = Awaiting::FindTarget {
                    forward: true,
                    till: true,
                };
                return Feed::Pending;
            }
            KeyCode::Char('T') => {
                self.normal.awaiting = Awaiting::FindTarget {
                    forward: false,
                    till: true,
                };
                return Feed::Pending;
            }
            KeyCode::Char(';') => {
                if let Some((ch, forward, till)) = self.last_find {
                    return self.motion(Motion::FindChar { ch, forward, till });
                }
            }
            KeyCode::Char(',') => {
                if let Some((ch, forward, till)) = self.last_find {
                    // `,` repeats in the opposite direction.
                    return self.motion(Motion::FindChar {
                        ch,
                        forward: !forward,
                        till,
                    });
                }
            }
            // Line jumps: `g` arms `gg`; `G` jumps to `{count}` (or the last line when no count).
            KeyCode::Char('g') => {
                self.normal.awaiting = Awaiting::GSecond;
                return Feed::Pending;
            }
            KeyCode::Char('G') => {
                return if self.normal.count > 0 {
                    self.motion(Motion::GotoLine)
                } else {
                    self.motion(Motion::LastLine)
                };
            }
            // Bare `%` matches the bracket; `{count}%` is Vim's percentage jump (go to count% of the file).
            KeyCode::Char('%') => {
                return if self.normal.count > 0 {
                    self.motion(Motion::GotoPercent)
                } else {
                    self.motion(Motion::MatchBracket)
                };
            }
            // `]` / `[` — arm a bracket-command prefix; the next key selects the command. Only the indent-
            // adjusting pastes `]p`/`]P`/`[p`/`[P` are wired. The count (`3]p`) is preserved for it.
            KeyCode::Char(']') => {
                self.normal.awaiting = Awaiting::BracketPrefix {
                    open_bracket: false,
                };
                return Feed::Pending;
            }
            KeyCode::Char('[') => {
                self.normal.awaiting = Awaiting::BracketPrefix { open_bracket: true };
                return Feed::Pending;
            }
            // `"x` — arm register selection. Deliberately does NOT reset the count/operator axes, so a
            // count typed after it still lands (`"a3yy`). The next key is the register name (see the
            // `RegisterSelect` tier above). Shared by Normal and Visual (Vim supports `"ayiw` and `"xy`).
            KeyCode::Char('"') => {
                self.normal.awaiting = Awaiting::RegisterSelect;
                return Feed::Pending;
            }
            // `` ` `` — arm a mark jump; the next key names the mark (`.` = last change, or `a`–`z`).
            KeyCode::Char('`') => {
                self.normal.awaiting = Awaiting::MarkJump;
                return Feed::Pending;
            }
            // `m` — arm a mark SET; the next key names the mark (`a`–`z`).
            KeyCode::Char('m') => {
                self.normal.awaiting = Awaiting::SetMarkChar;
                return Feed::Pending;
            }
            // `'` — arm a LINEWISE mark jump; the next key names the mark (`.` or `a`–`z`).
            KeyCode::Char('\'') => {
                self.normal.awaiting = Awaiting::MarkJumpLine;
                return Feed::Pending;
            }
            _ => {}
        }
        // Visual and Select: the selection already exists, so operators act on it directly and motions
        // extend it. The two share every matched key here (identical selection state); they diverge ONLY
        // in the unmatched-key fallthrough — Visual ignores, Select replaces-and-inserts.
        //
        // `gv` (restore the previous selection) IS wired — it re-selects the depth-1 `last_visual` (handled
        // in the `g`-initiator tier above); the full C-ANCHOR position history stays deferred.
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if let Mode::Visual { kind } | Mode::Select { kind } = mode {
            // `v`/`V`/`CTRL-V` switch the selection SHAPE: pressing the key of the current shape leaves the
            // namespace (to Normal), any other switches to that shape (F-025 c1). `i`/`a` (lowercase) begin
            // a text object in every shape; blockwise-only `I`/`A` (and block `c`/`s`) arm an insert-
            // replicate session instead of the plain charwise change.
            let is_block = kind == SelectKind::Blockwise;
            let shape_toggle = |target: SelectKind| {
                if kind == target {
                    Command::EnterNormal
                } else {
                    Command::EnterVisual { kind: target }
                }
            };
            match key.code {
                KeyCode::Esc => return self.action(Command::EnterNormal),
                KeyCode::Char('v') if ctrl => {
                    return self.action(shape_toggle(SelectKind::Blockwise))
                }
                KeyCode::Char('v') => return self.action(shape_toggle(SelectKind::Charwise)),
                KeyCode::Char('V') => return self.action(shape_toggle(SelectKind::Linewise)),
                // Blockwise insert-replicate: `I` at the left edge, `A` at the right edge, `c`/`s` delete
                // the block then insert at the left edge — each replicates on `<Esc>` (blockwise slice 2).
                KeyCode::Char('I') if is_block => {
                    return self.action(Command::BlockInsert(BlockInsertKind::Insert))
                }
                KeyCode::Char('A') if is_block => {
                    return self.action(Command::BlockInsert(BlockInsertKind::Append))
                }
                KeyCode::Char('c') | KeyCode::Char('s') if is_block => {
                    return self.action(Command::BlockInsert(BlockInsertKind::Change))
                }
                // Visual `CTRL-A`/`CTRL-X` — add ±count to the first number on EVERY selected line. Guarded
                // by `ctrl` and placed BEFORE the plain `x` delete arm so the ctrl-modified key wins.
                KeyCode::Char('a') if ctrl => {
                    return self.action(Command::IncrementSelection {
                        delta: i64::from(self.mcount()),
                        sequential: false,
                    })
                }
                KeyCode::Char('x') if ctrl => {
                    return self.action(Command::IncrementSelection {
                        delta: -i64::from(self.mcount()),
                        sequential: false,
                    })
                }
                KeyCode::Char('d') | KeyCode::Char('x') => {
                    return self.action(Command::DeleteSelection)
                }
                KeyCode::Char('y') => return self.action(Command::YankSelection),
                KeyCode::Char('c') | KeyCode::Char('s') => {
                    return self.action(Command::ChangeSelection)
                }
                // Visual `u`/`U`/`~` recase the selection (Visual only; in Select a printable key replaces
                // it via the namespace policy). `gu`/`gU`/`g~` in Visual are a follow-up.
                KeyCode::Char('u') if matches!(mode, Mode::Visual { .. }) => {
                    return self.action(Command::CaseSelection(WordCase::Downcase))
                }
                KeyCode::Char('U') if matches!(mode, Mode::Visual { .. }) => {
                    return self.action(Command::CaseSelection(WordCase::Upcase))
                }
                KeyCode::Char('~') if matches!(mode, Mode::Visual { .. }) => {
                    return self.action(Command::CaseSelection(WordCase::Toggle))
                }
                // `g` prefix in Visual: arm `GSecond` so `gu`/`gU`/`g~` recase the selection and the
                // `g`-motions (`gg`/`ge`/`gE`/`g_`) extend it. Was previously swallowed by the ignore policy.
                // Select mode is excluded: there a printable key REPLACES the selection (namespace policy).
                KeyCode::Char('g') if matches!(mode, Mode::Visual { .. }) => {
                    self.normal.awaiting = Awaiting::GSecond;
                    return Feed::Pending;
                }
                // Visual `r{char}` — replace every selected char with the next key (Vim `v_r`). Arm the
                // shared ReplaceChar expectation; its resolution sees the selection mode and emits the
                // selection form. Keeps the count axis (unused here) like Normal `r`.
                KeyCode::Char('r') => {
                    self.normal.awaiting = Awaiting::ReplaceChar;
                    return Feed::Pending;
                }
                // Visual `p`/`P` — replace the selection with the register. `p` swaps the deleted text into
                // the unnamed register; `P` preserves it (paste the same thing over successive selections).
                KeyCode::Char('p') => return self.action(Command::PasteSelection { swap: true }),
                KeyCode::Char('P') => return self.action(Command::PasteSelection { swap: false }),
                // `o` swaps the selection's ends (cursor <-> anchor); the SAME text stays selected but a
                // later motion extends the other end. In Normal `o` is OpenBelow — here it is the swap.
                KeyCode::Char('o') => return self.action(Command::SwapSelectionEnds),
                // In a selection, `i`/`a` always begin a text object (there is no insert here); the next key
                // is its selector. The completed object re-spans the selection (see the core's `Move` arm).
                KeyCode::Char('i') => {
                    self.normal.awaiting = Awaiting::TextObjectChar { inner: true };
                    return Feed::Pending;
                }
                KeyCode::Char('a') => {
                    self.normal.awaiting = Awaiting::TextObjectChar { inner: false };
                    return Feed::Pending;
                }
                // Count digits and motions extend the selection; an unmatched key hits the namespace's
                // own policy — `closed/ignore` for Visual, `open/replace-selection` for Select.
                KeyCode::Char('1'..='9') => {}
                KeyCode::Char('0') if self.normal.count > 0 => {}
                KeyCode::Char('0') => return self.motion(Motion::LineStart),
                _ if motion_key(key.code).is_some() => {}
                _ => {
                    let ns = if matches!(mode, Mode::Select { .. }) {
                        Ns::Select
                    } else {
                        Ns::Visual
                    };
                    return self.unmatched(ns, key);
                }
            }
            // fall through to shared count/motion handling below (op is never set in Visual/Select)
        }
        match key.code {
            KeyCode::Char(d @ '1'..='9') => {
                self.normal.count = self.normal.count.saturating_mul(10) + (d as u32 - '0' as u32);
                Feed::Pending
            }
            KeyCode::Char('0') if self.normal.count > 0 => {
                self.normal.count = self.normal.count.saturating_mul(10);
                Feed::Pending
            }
            KeyCode::Char('0') => self.motion(Motion::LineStart),
            code if motion_key(code).is_some() => {
                self.motion(motion_key(code).expect("guarded by is_some"))
            }
            // With an operator armed, `v`/`V`/`CTRL-V` FORCE the next motion's wise (Vim `o_v`/`o_V`/
            // `o_CTRL-V`): `dvj`, `dVe`, `d<C-v>j`. They stay operator-pending (the motion still follows);
            // `motion` emits `OpForced`. Bare (no operator) they enter Visual/Visual-line/Visual-block.
            KeyCode::Char('v') if self.normal.op.is_some() && ctrl => {
                self.normal.forced_wise = Some(ForcedWise::Blockwise);
                Feed::Pending
            }
            KeyCode::Char('v') if self.normal.op.is_some() => {
                self.normal.forced_wise = Some(ForcedWise::Charwise);
                Feed::Pending
            }
            KeyCode::Char('V') if self.normal.op.is_some() => {
                self.normal.forced_wise = Some(ForcedWise::Linewise);
                Feed::Pending
            }
            KeyCode::Char('v') if ctrl => self.action(Command::EnterVisual {
                kind: SelectKind::Blockwise,
            }),
            KeyCode::Char('v') => self.action(Command::EnterVisual {
                kind: SelectKind::Charwise,
            }),
            KeyCode::Char('V') => self.action(Command::EnterVisual {
                kind: SelectKind::Linewise,
            }),
            KeyCode::Char('d') => self.operator(Op::Delete, Command::Delete),
            KeyCode::Char('c') => self.operator(Op::Change, Command::Change),
            KeyCode::Char('y') => self.operator(Op::Yank, Command::Yank),
            // `>`/`<` are operators: doubled (`>>`) shifts the current `count` lines, and over a motion
            // (`>j`, `>ap`) shifts the motion's lines (always linewise).
            KeyCode::Char('>') => self.operator(Op::ShiftRight, |n, _| Command::ShiftRight(n)),
            KeyCode::Char('<') => self.operator(Op::ShiftLeft, |n, _| Command::ShiftLeft(n)),
            // `=` reindents (bracket-depth): doubled `==` is linewise, `=motion`/`=ap` over a motion.
            KeyCode::Char('=') => self.operator(Op::Reindent, |n, _| Command::Reindent {
                count: n,
                motion: Motion::Line,
            }),
            // `!` — the filter operator. Doubled `!!` (and `{count}!!`) filters the current `count` lines
            // (like `>>`/`gqq`); this guard must precede the arming arm below. The frontend numbers the lines
            // and opens the `:{range}!` cmdline. `count` folds the operator count with the doubling key's.
            KeyCode::Char('!')
                if matches!(self.normal.op, Some(OpPending { op: Op::Filter, .. })) =>
            {
                let count = match self.normal.op {
                    Some(OpPending { count, .. }) => count.max(1) * self.mcount(),
                    None => unreachable!("Op::Filter is armed"),
                };
                self.reset();
                Feed::FilterMotion {
                    count,
                    motion: Motion::Line,
                }
            }
            // `!{motion}{cmd}<CR>` — arm the filter operator over the next motion (linewise, like `=`/`gq`).
            KeyCode::Char('!') if self.normal.op.is_none() => {
                self.normal.op = Some(OpPending {
                    op: Op::Filter,
                    count: self.mcount(),
                });
                self.normal.count = 0;
                Feed::Pending
            }
            KeyCode::Char('p') => self.action(Command::Paste {
                after: true,
                count: self.mcount(),
                move_after: false,
            }),
            KeyCode::Char('P') => self.action(Command::Paste {
                after: false,
                count: self.mcount(),
                move_after: false,
            }),
            // Line-operator synonyms: `D`=`d$`, `C`=`c$`, `Y`=`y$` (nvim 0.6+ charwise), `S`=`cc`.
            // Each is the existing operator applied to an implicit motion, routed through the same
            // plan/commit path (so register geometry, cursor clamping, and dot-replayability match).
            KeyCode::Char('D') => self.action(Command::Delete(self.mcount(), Motion::LineEnd)),
            KeyCode::Char('C') => self.action(Command::Change(self.mcount(), Motion::LineEnd)),
            KeyCode::Char('Y') => self.action(Command::Yank(self.mcount(), Motion::LineEnd)),
            KeyCode::Char('S') => self.action(Command::Change(self.mcount(), Motion::Line)),
            // `s` (substitute char) = `cl`: change `count` chars rightward and enter Insert. Same
            // `Change(count, Right)` a `c l` produces, so register geometry and dot-repeat match. Guarded
            // to Normal (no pending operator) so `ds` still aborts — `s` is not a motion.
            KeyCode::Char('s') if self.normal.op.is_none() => {
                self.action(Command::Change(self.mcount(), Motion::Right))
            }
            // `CTRL-A` / `CTRL-X` — increment / decrement the number under-or-after the cursor by the count.
            // Placed before the plain `a`/`x` arms so the ctrl-modified keys are not swallowed by them.
            KeyCode::Char('a') if ctrl => {
                self.action(Command::IncrementNumber(i64::from(self.mcount())))
            }
            KeyCode::Char('x') if ctrl => {
                self.action(Command::IncrementNumber(-i64::from(self.mcount())))
            }
            // `CTRL-O` / `CTRL-I` — walk the jumplist back / forward (before the plain `o`/`i` arms).
            KeyCode::Char('o') if ctrl => self.action(Command::GotoOlderJump),
            KeyCode::Char('i') if ctrl => self.action(Command::GotoNewerJump),
            KeyCode::Char('i') if self.normal.op.is_some() => {
                self.normal.awaiting = Awaiting::TextObjectChar { inner: true };
                Feed::Pending
            }
            KeyCode::Char('a') if self.normal.op.is_some() => {
                self.normal.awaiting = Awaiting::TextObjectChar { inner: false };
                Feed::Pending
            }
            KeyCode::Char('i') => self.insert_entry(Command::EnterInsert),
            KeyCode::Char('a') => self.insert_entry(Command::EnterInsertAfter),
            KeyCode::Char('I') => self.insert_entry(Command::InsertLineStart),
            KeyCode::Char('A') => self.insert_entry(Command::AppendLineEnd),
            KeyCode::Char('o') => self.insert_entry(Command::OpenBelow),
            KeyCode::Char('O') => self.insert_entry(Command::OpenAbove),
            KeyCode::Char('x') => self.action(Command::DeleteUnder(self.mcount())),
            // Doubled case operators (`guu` / `gUU` / `g~~`): the second key repeats the operator to make
            // it linewise. These guards must precede the plain `u`/`~` handlers.
            KeyCode::Char('u')
                if matches!(
                    self.normal.op,
                    Some(OpPending {
                        op: Op::CaseLower,
                        ..
                    })
                ) =>
            {
                self.case_linewise()
            }
            KeyCode::Char('U')
                if matches!(
                    self.normal.op,
                    Some(OpPending {
                        op: Op::CaseUpper,
                        ..
                    })
                ) =>
            {
                self.case_linewise()
            }
            KeyCode::Char('~')
                if matches!(
                    self.normal.op,
                    Some(OpPending {
                        op: Op::CaseToggle,
                        ..
                    })
                ) =>
            {
                self.case_linewise()
            }
            // `g??` — the doubled ROT13 operator (linewise). `?` is not otherwise bound in Normal (backward
            // search is unwired), so this guard is the only meaning of `?` after `g?` armed the operator.
            KeyCode::Char('?')
                if matches!(self.normal.op, Some(OpPending { op: Op::Rot13, .. })) =>
            {
                self.case_linewise()
            }
            KeyCode::Char('u') => self.action(Command::Undo),
            KeyCode::Char('r') if ctrl => self.action(Command::Redo),
            KeyCode::Tab => self.action(Command::GotoNewerJump),
            KeyCode::Char('r') => {
                self.normal.awaiting = Awaiting::ReplaceChar;
                Feed::Pending
            }
            KeyCode::Char('R') => self.action(Command::EnterReplace),
            KeyCode::Char('~') => self.action(Command::ToggleCase(self.mcount())),
            KeyCode::Char('J') => self.action(Command::JoinLines(self.mcount())),
            // Bare `&` — repeat the last `:s` on the CURRENT LINE, dropping its flags (Vim: only the first
            // match on the cursor's line is replaced). The whole-file, flag-keeping form is `g&`
            // (`RepeatSubstituteGlobal`, in the `g` tier). Frontend-resolved against the last-`:s` state.
            KeyCode::Char('&') => self.action(Command::RepeatSubstituteLine),
            // `n` repeats the last search in the SAME direction it was issued; `N` in the OPPOSITE. So
            // after `?foo`, `n` continues BACKWARD and `N` goes forward; after `/foo` they stay
            // forward-relative. Direction comes from the stored last-search flag. Both REAPPLY the stored
            // offset (`:help search-offset`), so they route through [`Command::Search`] (which carries the
            // offset) rather than the plain `SearchNext`/`SearchPrev`; the offset-aware match rule advances
            // past the current match even when the offset landed the cursor before it (`s-1`).
            KeyCode::Char('n') => self.repeat_search(false, key),
            KeyCode::Char('N') => self.repeat_search(true, key),
            // `*`/`#` — search the whole keyword under the cursor forward / backward. The frontend reads
            // the word from the buffer and rewrites this to a concrete search (the engine has no buffer).
            KeyCode::Char('*') => self.action(Command::SearchWordUnder {
                forward: true,
                whole_word: true,
            }),
            KeyCode::Char('#') => self.action(Command::SearchWordUnder {
                forward: false,
                whole_word: true,
            }),
            // Dot-repeat: replay the last recorded change at the current cursor (D-047). A leading `N`
            // overrides the change's count (Vim `3.`). `.` itself never rewrites the record, so `..`
            // repeats the same change; with no prior change it is a clean no-op (the Normal namespace's
            // `closed/ignore` policy — Vim rings the bell).
            KeyCode::Char('.') => {
                if let Some(intent) = self.last_change.clone() {
                    let count = (self.normal.count > 0).then_some(self.normal.count);
                    self.reset();
                    Feed::Replay(intent.replay(count))
                } else {
                    self.unmatched(Ns::Normal, key)
                }
            }
            KeyCode::Char('/') => self.enter_search(false),
            // `?` — open the BACKWARD search line. Reachable here only when no ROT13 operator is pending
            // (the guarded `g??` arm above wins that case); an armed d/c/y operator is captured so `d?pat`
            // works, exactly like `d/pat`.
            KeyCode::Char('?') => self.enter_search(true),
            KeyCode::Char(':') => {
                self.reset();
                self.open_cmdline(':', false);
                Feed::Pending
            }
            // The base namespace's own declared policy — not a shared fallthrough. In Visual this
            // line is unreachable (the Visual arm above returns first), which is why the two are
            // separate calls rather than one `mode`-derived namespace.
            _ => self.unmatched(Ns::Normal, key),
        }
    }

    fn feed_impl(&mut self, key: KeyEvent, mode: Mode) -> Feed {
        // KL-OBL-4: a layer's owned state is destroyed when the layer deactivates — the engine does not
        // reach in to reset foreign fields.
        //
        // The Insert layer owns the `CTRL-G` prefix; it dies the moment the active namespace is not
        // Insert (a key in any other mode means the insert context is gone). A pending Insert one-shot
        // return (KL-OBL-5) is likewise abandoned — its resume address no longer applies.
        if mode != Mode::Insert {
            self.insert = InsertState::default();
            self.activations.retain(|a| a.resume != Ns::Insert);
        }
        // The Normal-family grammar layer (Normal / Visual / Select, with operator-pending as its
        // sub-state) owns count / operator / awaiting / forced-wise. They die when neither the family
        // nor an `i_CTRL-O` one-shot — which runs a single Normal command from WITHIN Insert, so the
        // family is momentarily active there — is in effect.
        let normal_family = matches!(
            mode,
            Mode::Normal | Mode::Visual { .. } | Mode::Select { .. }
        );
        if !normal_family && !self.in_one_shot() {
            self.normal = NormalState::default();
        }
        // Insert resolves through its LAYER, not through an early return ahead of everything else.
        // The bindings and the `open/insert` policy both live in `VimProfile`, so the namespace is
        // addressable in its own right (KL-OBL-1) and its policy is declared (KL-OBL-2).
        //
        // Two multi-key insert sequences are handled BEFORE the layer: `CTRL-O` (push a one-shot Normal
        // activation, then fall through to the Normal grammar for the rest of this and following keys until
        // it completes) and `CTRL-G u` (undo-break). A `CTRL-O` already in flight (on the activation stack)
        // skips the insert branch entirely so the pending Normal command keeps resolving.
        if mode == Mode::Insert && !self.in_one_shot() {
            return self.feed_insert(key);
        }
        if mode == Mode::Replace || mode == Mode::VirtualReplace {
            return self.feed_replace(key, mode);
        }
        // --- Native leader tier (F-013 NAT-2), ABOVE the Vim grammar and gated to the Native profile. ---
        // An ARMED leader consumes this key as its which-key selection (a bound command, or a which-key
        // abort). `self.leader` is only ever set on the Native+Normal path, so this is inert elsewhere.
        if self.leader {
            self.leader = false;
            return native_leader_command(key).map_or(Feed::Ignored, Feed::Cmd);
        }
        // ARM the leader from a CLEAN Normal base: `<leader>` (Space) opens the menu. Gated to Native +
        // Normal + clean so Vim's Space=MoveRight and a mid-construct Space-as-motion stay intact (NAT-1).
        if self.input_profile == InputProfile::Native
            && mode == Mode::Normal
            && self.normal.is_clean()
            && key.code == KeyCode::Char(' ')
            && key.modifiers.is_empty()
        {
            self.leader = true;
            return Feed::Pending;
        }
        // --- Top-priority tier: a one-shot key-expectation resolves before any base-key handling. ---
        match self.normal.awaiting {
            Awaiting::FindTarget { forward, till } => {
                self.normal.awaiting = Awaiting::Nothing;
                return match key.code {
                    KeyCode::Char(ch) => {
                        self.last_find = Some((ch, forward, till));
                        self.motion(Motion::FindChar { ch, forward, till })
                    }
                    // A pending construct is in flight, so this is `closed/abort` — the policy
                    // that distinguishes operator-pending from Normal (VS-OBL-3).
                    _ => self.unmatched(Ns::OperatorPending, key),
                };
            }
            Awaiting::TextObjectChar { inner } => {
                self.normal.awaiting = Awaiting::Nothing;
                return match key.code {
                    // Under an operator this composes (`diw`/`da(`/`ci"`); in a selection `self.motion`
                    // emits a bare `Move` whose text-object shape the core turns into a selection (`viw`).
                    KeyCode::Char(ch) if text_object(ch, inner).is_some() => {
                        self.motion(text_object(ch, inner).expect("guarded by is_some"))
                    }
                    // Not a text-object selector (`t` IS one — `it`/`at` tag objects resolve via
                    // `text_object` → `Motion::Tag` above): a pending construct is in flight, so this is
                    // `closed/abort` — the operator-pending policy (VS-OBL-3).
                    _ => self.unmatched(Ns::OperatorPending, key),
                };
            }
            Awaiting::GSecond => {
                self.normal.awaiting = Awaiting::Nothing;
                let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
                return match key.code {
                    // `g CTRL-G` — cursor position / buffer counts (Col/Line/Word/[Char]/Byte). Checked
                    // before the bare `g` (=`gg`) arm, since that arm ignores the ctrl modifier. Frontend-
                    // resolved + status-surfaced; no buffer mutation.
                    KeyCode::Char('g') if ctrl => self.action(Command::CursorInfo),
                    // `ga` — the numeric value of the character under the cursor (`:ascii`/`:as` synonym).
                    // `!ctrl` keeps it distinct from the Visual `g CTRL-A` sequence-increment arm below.
                    KeyCode::Char('a') if !ctrl => self.action(Command::AsciiInfo),
                    KeyCode::Char('g') => self.motion(Motion::GotoLine),
                    // `ge` / `gE` — backward to the end of the previous word / WORD (operator-aware via
                    // `motion`, so `dge` deletes back through the previous word-end).
                    KeyCode::Char('e') => self.motion(Motion::WordEndBack),
                    KeyCode::Char('E') => self.motion(Motion::BigWordEndBack),
                    // `g_` — to the last non-blank char of the line (`{count}g_` = count-1 lines down).
                    KeyCode::Char('_') => self.motion(Motion::LineLastNonBlank),
                    // Display-line motions `gj`/`gk`/`g0`/`g$`/`g^`. ruse does NOT soft-wrap (each buffer
                    // line is exactly one display row — `paint_pane` truncates at the right edge, the
                    // viewport is a single vertical `top` offset with no horizontal scroll), so a buffer
                    // line and its display line always coincide. Vim's `gj`/`gk`/`g0`/`g$`/`g^` therefore
                    // equal `j`/`k`/`0`/`$`/`^` as BARE CURSOR MOTIONS here — the same equivalence Vim
                    // itself gives under `nowrap` (`:help gj`). They are emitted as those motions so the
                    // keys stop being dead: count-aware (`3gj`) and operator-composable via `motion`.
                    //
                    // DELIBERATE DIVERGENCE (operator forms only): `dgj`/`dgk` alias `dj`/`dk` (linewise),
                    // whereas nvim treats `gj`/`gk` as characterwise-exclusive and applies its exclusive-
                    // linewise promotion, so nvim's `dgj` deletes ONE line, not two. A column-preserving
                    // charwise vertical motion (with desired-column tracking) is out of scope for wiring
                    // these keys; the horizontal forms (`g0`/`g$`/`g^`) match nvim under operators exactly.
                    KeyCode::Char('j') => self.motion(Motion::Down),
                    KeyCode::Char('k') => self.motion(Motion::Up),
                    KeyCode::Char('0') => self.motion(Motion::LineStart),
                    KeyCode::Char('$') => self.motion(Motion::LineEnd),
                    KeyCode::Char('^') => self.motion(Motion::LineFirstNonBlank),
                    // `[count]go` — go to the count-th byte of the buffer (operator-aware: `dgo`).
                    KeyCode::Char('o') => self.motion(Motion::GotoByte),
                    // `gM` — go to `count`% of the line by CHARACTER count (bare `gM` = the middle char,
                    // i.e. 50%). The count is a PERCENTAGE, not a repeat, so seed it into `count` (default 50)
                    // and let `motion` carry it through — exactly the count-as-percentage shape `{count}%`
                    // (`GotoPercent`) uses. Operator-aware (`dgM`); `op_over_motion` clears the count after.
                    //
                    // `gm` (middle of the SCREEN line, i.e. the char at display column `window_width/2`) is
                    // DEFERRED: it is a pure function of the window width (nvim goes to the last char once the
                    // half-width column runs past the line), which the motion engine has no access to at motion
                    // time — the frontend never threads viewport width into `Motion`. Aliasing it to `gM`
                    // would be wrong (on a 10-char line in an 80-col window nvim's `gm` lands on the last
                    // char, `gM` on the 6th), so `gm` is left unbound (aborts as operator-pending) until a
                    // width-aware motion path exists.
                    KeyCode::Char('M') => {
                        if self.normal.count == 0 {
                            self.normal.count = 50; // bare `gM` = 50% of the line
                        }
                        self.motion(Motion::MidLine)
                    }
                    // `g*` / `g#` — like `*`/`#` but match the word ANYWHERE (no `\<…\>` boundaries).
                    KeyCode::Char('*') => self.action(Command::SearchWordUnder {
                        forward: true,
                        whole_word: false,
                    }),
                    KeyCode::Char('#') => self.action(Command::SearchWordUnder {
                        forward: false,
                        whole_word: false,
                    }),
                    // `gd` / `gD` — go to the local / global declaration of the keyword under the cursor via
                    // Vim's TEXT heuristic (NOT LSP; ruse's LSP goto is separate). The engine has no buffer,
                    // so — exactly like `*`/`#` — the frontend reads the word and rewrites this to a concrete
                    // whole-file first-match jump. Both land on the first whole-word match from the top of the
                    // file, matching nvim v0.12.4 where `gd` and `gD` are identical (verified); `global` marks
                    // `gD` and reserves the `gd` enclosing-block refinement as a follow-up.
                    KeyCode::Char('d') => self.action(Command::GotoDeclaration { global: false }),
                    KeyCode::Char('D') => self.action(Command::GotoDeclaration { global: true }),
                    // In Visual, `gu`/`gU`/`g~` recase the selection immediately (same as bare `u`/`U`/`~`).
                    KeyCode::Char('u') if matches!(mode, Mode::Visual { .. }) => {
                        self.action(Command::CaseSelection(WordCase::Downcase))
                    }
                    KeyCode::Char('U') if matches!(mode, Mode::Visual { .. }) => {
                        self.action(Command::CaseSelection(WordCase::Upcase))
                    }
                    KeyCode::Char('~') if matches!(mode, Mode::Visual { .. }) => {
                        self.action(Command::CaseSelection(WordCase::Toggle))
                    }
                    KeyCode::Char('?') if matches!(mode, Mode::Visual { .. }) => {
                        self.action(Command::CaseSelection(WordCase::Rot13))
                    }
                    // Visual `g CTRL-A` / `g CTRL-X` — increment the selected lines as a SEQUENCE: the first
                    // numbered line gets ±count, the next ±2·count, and so on (make a column of 1s into 1,2,3…).
                    KeyCode::Char('a') if ctrl && matches!(mode, Mode::Visual { .. }) => self
                        .action(Command::IncrementSelection {
                            delta: i64::from(self.mcount()),
                            sequential: true,
                        }),
                    KeyCode::Char('x') if ctrl && matches!(mode, Mode::Visual { .. }) => self
                        .action(Command::IncrementSelection {
                            delta: -i64::from(self.mcount()),
                            sequential: true,
                        }),
                    // `gu` / `gU` / `g~` / `g?` — arm a case operator (lower / upper / toggle / ROT13) over
                    // the next motion. Only from Normal (no operator already pending): `dgu` is not Vim.
                    KeyCode::Char('u') if self.normal.op.is_none() => {
                        self.arm_case_op(Op::CaseLower)
                    }
                    KeyCode::Char('U') if self.normal.op.is_none() => {
                        self.arm_case_op(Op::CaseUpper)
                    }
                    KeyCode::Char('~') if self.normal.op.is_none() => {
                        self.arm_case_op(Op::CaseToggle)
                    }
                    KeyCode::Char('?') if self.normal.op.is_none() => self.arm_case_op(Op::Rot13),
                    // `gq` / `gw` — arm the reflow operator over the next motion (`gqap`, `gwj`). Normal
                    // only; the doubled `gqq`/`gqgq` line form is deferred (it collides with macro `q`).
                    KeyCode::Char('q') if matches!(mode, Mode::Normal) => {
                        self.arm_case_op(Op::Format)
                    }
                    KeyCode::Char('w') if matches!(mode, Mode::Normal) => {
                        self.arm_case_op(Op::FormatKeep)
                    }
                    // `gn` / `gN` — the search-match text object: select (bare) or operate on (`dgn`/`cgn`/
                    // `ygn`) the next / previous match of the last search pattern. Operator-aware.
                    KeyCode::Char('n') => self.search_object(false),
                    KeyCode::Char('N') => self.search_object(true),
                    // `gJ` — join with the next line WITHOUT inserting a space (Vim `gJ`).
                    KeyCode::Char('J') => self.action(Command::JoinLinesNoSpace(self.mcount())),
                    // `g;` / `g,` — walk the change list to older / newer change positions (Vim).
                    KeyCode::Char(';') => self.action(Command::GotoOlderChange),
                    KeyCode::Char(',') => self.action(Command::GotoNewerChange),
                    // `gi` — resume Insert at the last-insert position (Vim `` `^ ``).
                    KeyCode::Char('i') => self.action(Command::InsertAtLastInsert),
                    // `gp` / `gP` — paste like `p`/`P` but leave the cursor JUST AFTER the pasted text.
                    KeyCode::Char('p') => self.action(Command::Paste {
                        after: true,
                        count: self.mcount(),
                        move_after: true,
                    }),
                    KeyCode::Char('P') => self.action(Command::Paste {
                        after: false,
                        count: self.mcount(),
                        move_after: true,
                    }),
                    // `g&` — repeat the last `:s` over the whole file with its flags (frontend-resolved).
                    KeyCode::Char('&') => self.action(Command::RepeatSubstituteGlobal),
                    // `gv` — re-select the last visual selection (D-027 depth-1 slice).
                    KeyCode::Char('v') => self.action(Command::ReselectVisual),
                    // `gR` — enter Virtual Replace mode (tab-aware overwrite).
                    KeyCode::Char('R') => self.action(Command::EnterVirtualReplace),
                    // `g-` / `g+` — chronological undo-time travel across branches (F-005 #3).
                    KeyCode::Char('-') => self.action(Command::UndoOlder),
                    KeyCode::Char('+') => self.action(Command::UndoNewer),
                    // `gQ` — enter Ex mode (F-026 #3). `Q` alone is NOT Ex at the pinned Neovim
                    // revision, so only this two-key form opens it; the line re-prompts until `:visual`.
                    KeyCode::Char('Q') => {
                        self.open_cmdline(':', true);
                        Feed::Pending
                    }
                    // A pending construct is in flight, so this is `closed/abort` — the policy
                    // that distinguishes operator-pending from Normal (VS-OBL-3).
                    _ => self.unmatched(Ns::OperatorPending, key),
                };
            }
            Awaiting::ReplaceChar => {
                self.normal.awaiting = Awaiting::Nothing;
                return match key.code {
                    // In a selection, `r{char}` replaces EVERY selected char with it (Vim `v_r`); in Normal
                    // it replaces `count` chars under the cursor. The count accumulated before `r` is still
                    // live (the `r` arm did not reset it).
                    KeyCode::Char(c)
                        if matches!(mode, Mode::Visual { .. } | Mode::Select { .. }) =>
                    {
                        self.action(Command::ReplaceSelectionChar(c))
                    }
                    KeyCode::Char(c) => self.action(Command::ReplaceChar(self.mcount(), c)),
                    // `r<CR>` replaces the char(s) with a line break (Vim splits the line). Enter is a
                    // distinct KeyCode, so map it to a `\n` replacement here.
                    KeyCode::Enter if matches!(mode, Mode::Visual { .. } | Mode::Select { .. }) => {
                        self.action(Command::ReplaceSelectionChar('\n'))
                    }
                    KeyCode::Enter => self.action(Command::ReplaceChar(self.mcount(), '\n')),
                    // A pending construct is in flight, so this is `closed/abort` — the policy
                    // that distinguishes operator-pending from Normal (VS-OBL-3).
                    _ => self.unmatched(Ns::OperatorPending, key),
                };
            }
            Awaiting::MarkJump => {
                self.normal.awaiting = Awaiting::Nothing;
                return match key.code {
                    // `` `. `` — jump to the last-change mark.
                    KeyCode::Char('.') => self.action(Command::GotoLastChange),
                    // `` `` `` / `` `' `` — jump to the context mark (position before the latest jump).
                    KeyCode::Char('`') | KeyCode::Char('\'') => {
                        self.action(Command::GotoContextMark)
                    }
                    // `` `[ `` / `` `] `` — jump charwise to the first/last char of the last changed/yanked text.
                    KeyCode::Char('[') => self.action(Command::GotoChangeMarkStart),
                    KeyCode::Char(']') => self.action(Command::GotoChangeMarkEnd),
                    // `` `< `` / `` `> `` — jump charwise to the start/end of the last visual selection.
                    KeyCode::Char('<') => self.action(Command::GotoVisualMarkStart),
                    KeyCode::Char('>') => self.action(Command::GotoVisualMarkEnd),
                    // `` `{a-z} `` — jump to a named mark, or `` d`a ``/`` y`a `` with an operator pending.
                    KeyCode::Char(c @ 'a'..='z') => self.mark_op(c, false),
                    // Any other mark name is not wired — abort the pending construct.
                    _ => self.unmatched(Ns::OperatorPending, key),
                };
            }
            Awaiting::SetMarkChar => {
                self.normal.awaiting = Awaiting::Nothing;
                return match key.code {
                    // `m{a-z}` — set a named mark at the cursor.
                    KeyCode::Char(c @ 'a'..='z') => self.action(Command::SetNamedMark(c)),
                    // Uppercase/global marks and specials are deferred — abort.
                    _ => self.unmatched(Ns::OperatorPending, key),
                };
            }
            Awaiting::MarkJumpLine => {
                self.normal.awaiting = Awaiting::Nothing;
                return match key.code {
                    // `'.` — linewise to the last-change line.
                    KeyCode::Char('.') => self.action(Command::GotoLastChangeLine),
                    // `''` / `` '` `` — linewise to the context mark's line (position before the latest jump).
                    KeyCode::Char('\'') | KeyCode::Char('`') => {
                        self.action(Command::GotoContextMarkLine)
                    }
                    // `'[` / `']` — linewise to the first non-blank of the first/last changed/yanked line.
                    KeyCode::Char('[') => self.action(Command::GotoChangeMarkStartLine),
                    KeyCode::Char(']') => self.action(Command::GotoChangeMarkEndLine),
                    // `'<` / `'>` — linewise to the first non-blank of the first/last selected line.
                    KeyCode::Char('<') => self.action(Command::GotoVisualMarkStartLine),
                    KeyCode::Char('>') => self.action(Command::GotoVisualMarkEndLine),
                    // `'{a-z}` — linewise to a named mark's line, or `d'a`/`y'a` with an operator pending.
                    KeyCode::Char(c @ 'a'..='z') => self.mark_op(c, true),
                    // Any other mark name is not wired — abort.
                    _ => self.unmatched(Ns::OperatorPending, key),
                };
            }
            Awaiting::RegisterSelect => {
                self.normal.awaiting = Awaiting::Nothing;
                // `"=` — the expression register (`:help quote=`): open the expression prompt instead of
                // arming a stored slot. On `<CR>` the collected expression is evaluated and the `"=` register
                // armed, so the FOLLOWING `p`/`P` pastes the result. Reset first so no partial Normal state
                // (a stray count/operator) survives into the prompt.
                if key.code == KeyCode::Char('=') {
                    self.reset();
                    self.open_expr_prompt(ExprTarget::Paste);
                    return Feed::Pending;
                }
                return match key.code {
                    // A register name — a letter (`a`–`z` / `A`–`Z`), the yank register `0`, the numbered
                    // delete-ring `1`–`9`, the small-delete `-`, or the blackhole `_`: emit `SetRegister` for the core
                    // to hold as the pending register the next yank/delete/change/paste reads. `action`
                    // clears the transient axes — which is why the register PREFIX must precede a count
                    // (`"a3yy`, as in Vim). `"0p`/`"1p`/`"-p` paste from read-only edit-history slots.
                    KeyCode::Char(c)
                        if c.is_ascii_alphabetic()
                            || c.is_ascii_digit()
                            || c == '-'
                            || c == '_'
                            // `"+`/`"*` — the system clipboard (`:help quoteplus`).
                            || c == '+'
                            || c == '*'
                            // The read-only special registers `"/ ": ". "%` as a paste SOURCE (`"/p`,
                            // `".p`, `"%p`): arm them like any slot; the following `p`/`P` reads the
                            // frontend-synced value. A yank/delete naming one is swallowed (read-only).
                            || c == '/'
                            || c == ':'
                            || c == '.'
                            || c == '%' =>
                    {
                        self.action(Command::SetRegister(Some(c)))
                    }
                    // Any other name is unusable; a pending construct is in flight, so it is `closed/abort`
                    // (operator-pending), leaking no state.
                    _ => self.unmatched(Ns::OperatorPending, key),
                };
            }
            Awaiting::BracketPrefix { open_bracket } => {
                self.normal.awaiting = Awaiting::Nothing;
                let count = self.mcount();
                return match key.code {
                    // Indent-adjusting paste. Only `]p` pastes AFTER (below); `]P`, `[p`, `[P` all paste
                    // BEFORE (above), matching Vim. Other bracket commands (`[(`, `]}`, …) are not wired.
                    KeyCode::Char('p') => self.action(Command::PasteIndent {
                        after: !open_bracket,
                        count,
                    }),
                    KeyCode::Char('P') => self.action(Command::PasteIndent {
                        after: false,
                        count,
                    }),
                    // Section motions — count/operator-aware via `self.motion` (which reads the preserved
                    // count and composes any armed `d`/`c`/`y`). `]]`/`[[` seek a `{` (or form-feed) in
                    // column 0; `][`/`[]` seek a `}` (or form-feed). The starting bracket picks the axis.
                    KeyCode::Char(']') if !open_bracket => self.motion(Motion::SectionFwd),
                    KeyCode::Char('[') if !open_bracket => self.motion(Motion::SectionEndFwd),
                    KeyCode::Char('[') if open_bracket => self.motion(Motion::SectionBack),
                    KeyCode::Char(']') if open_bracket => self.motion(Motion::SectionEndBack),
                    // Unmatched-paren/brace motions — count/operator-aware via `self.motion`. `[(`/`[{` go to
                    // the previous unmatched `(`/`{`; `])`/`]}` go to the next unmatched `)`/`}`. The starting
                    // bracket (`[` vs `]`) fixes the direction; the pressed char picks paren vs brace.
                    KeyCode::Char('(') if open_bracket => self.motion(Motion::UnmatchedParenBack),
                    KeyCode::Char('{') if open_bracket => self.motion(Motion::UnmatchedBraceBack),
                    KeyCode::Char(')') if !open_bracket => self.motion(Motion::UnmatchedParenFwd),
                    KeyCode::Char('}') if !open_bracket => self.motion(Motion::UnmatchedBraceFwd),
                    _ => self.unmatched(Ns::OperatorPending, key),
                };
            }
            Awaiting::Nothing => {}
        }
        self.feed_base(key, mode)
    }
}

impl Default for InputEngine {
    /// Hand-written, NOT derived. A derived `Default` would build an empty layer set, every
    /// `resolve` would return `NoLayer`, and the policies would be silently disabled — the exact
    /// class of invisible regression the layer model exists to prevent.
    fn default() -> InputEngine {
        InputEngine::new()
    }
}

mod vim;
pub(crate) use vim::{motion_key, text_object, Ns, VimProfile};

mod native;
pub(crate) use native::{native_leader_command, NATIVE_LEADER_MENU};

mod emacs;
pub use emacs::emacs_command_by_name;
use emacs::{emacs_repeat, fold_emacs_count, EmacsArg, EmacsBinding, EmacsKey, EmacsProfile, Step};

mod cmdline;
mod cmdwin;
mod history;
use cmdline::{CmdLine, ExprTarget};
use cmdwin::CmdWin;
use history::CmdHistory;

pub(crate) mod digraph; // `pub(crate)`: the `:digraphs` listing overlay (ui) reads `digraph::entries`.
use digraph::digraph;

mod repeat;
use repeat::{change_kind, ChangeIntent, ChangeKind};

mod ex;
pub(crate) use ex::reuse_last_search;
// `GlobalPayload` is referenced by the non-test dispatch/run-loop (routing `:g/pat/normal` vs `d`/`s`) and
// by the integration scenarios that drive `:g` directly, so it is part of the input surface like `Ex`.
pub use ex::{parse_ex, BufTarget, Ex, GlobalPayload, ReadSource};
#[cfg(test)]
pub(crate) use ex::{parse_substitute, GlobalSpec, SubSpec};

/// Split a raw search line into `(pattern, offset)` on the LAST unescaped `delim` (`/` for a forward
/// search, `?` for a backward one) — Vim's `/{pattern}/{offset}` grammar (`:help search-offset`). A
/// delimiter preceded by an odd run of backslashes is escaped (part of the pattern), so `foo\/bar/e`
/// splits into `("foo\/bar", Some(e))`. A present-but-unrecognized offset parses to [`SearchOffset::None`].
///
/// The returned offset is `None` ONLY when NO delimiter is present (`foo`); a delimiter with an empty
/// offset part (`foo/`, or the `//` repeat) yields `Some(SearchOffset::None)`. This distinction matters
/// for the empty-pattern repeat: a bare `/<CR>` (no delimiter) reuses the last offset, whereas `//<CR>`
/// (a typed delimiter, empty offset) explicitly resets to NO offset — matching nvim (`/foo/e` then `//`
/// lands on the match start, but then `/<CR>` reuses `e`).
fn split_search_offset(line: &str, delim: char) -> (String, Option<SearchOffset>) {
    let bytes = line.as_bytes();
    let dl = delim as u8; // both `/` and `?` are single ASCII bytes
    let mut last: Option<usize> = None;
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' {
            i += 2; // skip the escaped char (a `\/` is a literal delimiter in the pattern)
            continue;
        }
        if bytes[i] == dl {
            last = Some(i);
        }
        i += 1;
    }
    match last {
        Some(idx) => (
            line[..idx].to_string(),
            Some(SearchOffset::parse(&line[idx + 1..])),
        ),
        None => (line.to_string(), None),
    }
}

#[cfg(test)]
mod unit_tests;
