//! The input engine: a small pending-state machine that folds keys into semantic [`Command`]s
//! (`d`, `2w`, `d3w`, `dd`, `cw`=`ce`), plus ex-command (`:…`) parsing. The trace records the resulting
//! commands, so re-keymapping never invalidates a corpus. Pure and unit-tested.

use std::collections::HashMap;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ruse_core::keymap::{Resolved, UnmatchedKey};
use ruse_core::VisualOp;
use ruse_core::{
    BlockInsertKind, Command, ForcedWise, GlobalCmd, LineAddr, MarkOp, Mode, Motion, OpKind,
    SearchOffset, SearchOp, SelectKind, SubFlags, SubRange, WordCase,
};

/// Neovim's count ceiling: a typed count, and an operator count times a motion count (`3d2w` = 6), clamp
/// here (`normal.c` — `99999999999w` and `99999y99999w` both report `v:count` = 999999999). Clamping also
/// keeps the `u32` arithmetic below from overflowing (#899), which would panic in debug and silently wrap
/// in a build without overflow checks.
pub(crate) const MAX_COUNT: u32 = 999_999_999;

/// Append decimal digit `d` to a count being typed, clamped to [`MAX_COUNT`].
fn push_count_digit(count: u32, d: u32) -> u32 {
    count.saturating_mul(10).saturating_add(d).min(MAX_COUNT)
}

/// Whether `key` is a chord: it carries a command modifier (CTRL, ALT, SUPER, HYPER or META). A chord never
/// matches an arm written for the bare key (#598, #599, #1059): `<C-s>` is not `s`. SHIFT is not a chord
/// modifier, because a real terminal reports an uppercase letter as SHIFT + `Q` (#681).
pub(crate) fn is_chord(key: KeyEvent) -> bool {
    key.modifiers.intersects(
        KeyModifiers::CONTROL
            | KeyModifiers::ALT
            | KeyModifiers::SUPER
            | KeyModifiers::HYPER
            | KeyModifiers::META,
    )
}

/// An unmodified key, for the CTRL aliases Vim resolves to another key (`<C-[>` = `<Esc>`, `<C-h>` =
/// `<BS>` in Insert). A legacy terminal sends some of them as that key already; the kitty protocol does not.
fn plain(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

/// A mark jump's feed as the `` g` ``/`g'` form when `keep` (#1175). Under an operator (`` dg`a ``) the
/// operator runs over the mark as for `` d`a ``, so the caller passes `keep` only for a bare jump.
fn keep_jumps_feed(feed: Feed, keep: bool) -> Feed {
    match feed {
        Feed::Cmd(cmd) if keep => Feed::KeepJumps(cmd),
        other => other,
    }
}

/// Whether `c` can open a terminal control sequence: ESC or a C1 control (U+0080..=U+009F, CSI and OSC
/// among them). A paste never carries one (TERM-PASTE-2, SEC-5).
#[must_use]
pub fn opens_control_sequence(c: char) -> bool {
    c == '\u{1b}' || ('\u{80}'..='\u{9f}').contains(&c)
}

/// A bracketed paste's text as ruse inserts it. Line endings are normalised the way nvim's TUI pastes
/// (`nvim_paste(data, crlf = true)`): CRLF and a lone CR both become LF. ESC and the C1 controls are
/// dropped (TERM-PASTE-2, SEC-5: a paste is untrusted and never carries a control sequence); nvim inserts
/// them literally. One pass, linear in the length.
#[must_use]
pub fn paste_text(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw
        .chars()
        .filter(|&c| !opens_control_sequence(c))
        .peekable();
    while let Some(c) = chars.next() {
        if c == '\r' {
            if chars.peek() == Some(&'\n') {
                chars.next();
            }
            out.push('\n');
        } else {
            out.push(c);
        }
    }
    out
}

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
    /// `` g`{mark} `` / `g'{mark}`: run the jump without changing the jumplist or the `''` mark (`:help g'`),
    /// as `:keepjumps` would run it. The frontend switches `keepjumps` on around it through the trace.
    KeepJumps(Command),
    /// A counted [`Repeat`] (`3ihello<Esc>`, `N.` of an insert, an Emacs `C-u N` prefix): applied like
    /// [`Feed::Replay`], one command at a time, but never expanded up front, so a huge count costs no memory
    /// and CTRL-C stops it between repetitions (#683).
    Repeat(Repeat),
    /// A completed `:`-line to execute (F-026). The command-line namespace owns the buffer while it is
    /// being typed; on `<CR>` it hands the finished text to the frontend to parse+run as an ex command.
    ExecuteEx(String),
    /// `c_CTRL-R_CTRL-W` / `c_CTRL-R_CTRL-A`: splice the `<cword>` (`big == false`) or `<cWORD>`
    /// (`big == true`) under the BUFFER cursor into the open command line at the caret. The namespace owns
    /// the cmdline buffer but the engine has no document, so the frontend resolves the word (via
    /// `Workspace::cword_under_cursor` / `cbig_word_under_cursor`) and splices it back with
    /// [`InputEngine::cmdline_splice`]. The command line stays open.
    CmdlineInsertUnder { big: bool },
    /// `c_CTRL-R_CTRL-L`: splice the BUFFER line under the cursor into the open command line at the caret
    /// (the frontend resolves it, as for [`Feed::CmdlineInsertUnder`]).
    CmdlineInsertLine,
    /// `c_CTRL-R {reg}` (and the literal `CTRL-R CTRL-R`/`CTRL-O {reg}` forms): splice register `reg` into
    /// the open command line at the caret. The engine has no registers, so the frontend reads it and calls
    /// [`InputEngine::cmdline_splice_register`]. The command line stays open.
    CmdlineInsertRegister(char),
    /// A command-line completion key with no wildmenu open (`:help cmdline-completion`): `<Tab>`,
    /// `<S-Tab>`, `c_CTRL-D`, `c_CTRL-L` or `c_CTRL-A`. Completion needs buffer names, the current file and
    /// option values the engine does not have, so the frontend answers with
    /// [`InputEngine::cmdline_complete`] and a [`complete::CompletionContext`]. The line stays open.
    CmdlineComplete(WildKey),
    /// `!{motion}` / `!!` (the Normal-mode filter operator): the motion resolved to a LINE range, but the
    /// engine has no document to number it and no shell to run. The frontend resolves the motion's first/last
    /// line (via `Workspace::reindent_range`), then opens the `:` command-line pre-seeded with
    /// `{first},{last}!` ([`InputEngine::open_filter_cmdline`]); typing the command and pressing `<CR>` runs
    /// the SAME `:{range}!{cmd}` ex filter. `count` is the folded operator × motion count.
    FilterMotion { count: u32, motion: Motion },
    /// `!` over a span command the engine cannot number either: a search (`!/pat<CR>`, `!n`), `!*`/`!#`,
    /// `!gn` or a mark (`!'a`, `` !`a ``) — #800. The frontend measures the command's span with
    /// `Workspace::span_lines` (the planner's own geometry, so `!` covers exactly the lines `d` would) and
    /// opens the same pre-seeded `{first},{last}!` command line as [`Feed::FilterMotion`]. The command is
    /// never applied; its operator is a non-mutating `Yank` placeholder.
    FilterSpan(Command),
    /// `{count}:` — open the `:` line on the `count` lines from the cursor's (Vim seeds `:.,.+{count-1}`).
    /// The engine has no document, so the frontend numbers the lines and seeds the absolute range with
    /// [`InputEngine::open_filter_cmdline`], as for [`Feed::FilterMotion`].
    CountCmdline { count: u32 },
    /// The key was consumed but the command is not complete yet (a count digit, a pending operator, or a
    /// keystroke absorbed into the open command-line buffer).
    Pending,
    /// Nothing bound.
    Ignored,
    /// A screen-line motion (`gj`, `gk`, `g0`, `g^`, `gm`, `g$`; #615) with its folded count: the frontend
    /// resolves where it lands in the window's screen rows and finishes it with
    /// [`InputEngine::screen_motion_to`], [`InputEngine::screen_motion_lines`] or
    /// [`InputEngine::screen_motion_fail`]. The operator, count and register stay armed until then.
    ScreenMotion { motion: ScreenMotion, count: u32 },
    /// Nothing bound, and Vim beeps (`nv_error`): the frontend records the failure, so the rest of a macro
    /// or a `:normal` is not run (#1111). `clear_register` abandons a `"x` prefix already handed to the
    /// core, as [`Feed::Ignored`]'s caller does with `Command::SetRegister(None)`.
    Beep { clear_register: bool },
}

/// A screen-line motion (`:help gj`): it moves over screen rows, not buffer lines (#615).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScreenMotion {
    /// `gj` / `g<Down>`.
    Down,
    /// `gk` / `g<Up>`.
    Up,
    /// `g0` / `g<Home>`.
    Start,
    /// `g^`.
    FirstNonBlank,
    /// `gm`.
    Middle,
    /// `g$` / `g<End>`.
    End,
}

/// The command-line completion keys (`:help cmdline-completion`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum WildKey {
    /// `<Tab>` (`'wildchar'`): complete, opening the wildmenu on the first of several matches.
    Next,
    /// `<S-Tab>`: the same, starting from the last match.
    Prev,
    /// `c_CTRL-D`: list the matches above the line.
    List,
    /// `c_CTRL-L`: insert the longest common part of the matches.
    Longest,
    /// `c_CTRL-A`: insert every match.
    All,
}

/// What the engine holds between two keys, as the session's frontend intercepts see it (#679, #850, #851).
/// The session resolves a few Normal keys itself (`z`, `C-w`, `&`, `H`/`M`/`L`, the scroll chords, the
/// pickers, the macro `q`/`@`/`Q`); each may take a key only when the engine is not waiting for it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Pending {
    /// A command boundary: a new command may start here. A count may be accumulating, and a `"x` register
    /// prefix may already be emitted ([`InputEngine::take_count`] consumes the count; every count segment
    /// folds through the same model the engine uses, #796).
    Base,
    /// An operator is armed (`d`, `gu`, `dv`, …) and waits for its motion, with no key expectation.
    Operator,
    /// `[` (`open`) or `]` waits for its second key. The session resolves `[z`/`]z` (fold moves) itself.
    Bracket { open: bool },
    /// The next key is an argument the engine owns: a find, replace, mark or register char, a text-object
    /// selector, the second key of `g`, a Native leader key, an Emacs prefix key, or command-line input.
    Argument,
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
    /// `gc` — comment / uncomment the operator span's lines with `'commentstring'` (#618).
    Comment,
    /// `!` — filter the operator span's LINES through a shell command (`!{motion}{cmd}<CR>`, `!!{cmd}`).
    /// Unlike the other operators this does not resolve to a buffer edit in the engine: the completed
    /// motion yields a [`Feed::FilterMotion`] the frontend turns into a pre-seeded `:{range}!` ex line
    /// (reusing the `:{range}!{cmd}` executor + `shell.rs`).
    Filter,
}

/// What an armed [`Op`] DOES, independent of the key that armed it. Every consumer (a motion, the doubled
/// linewise form, a search, `n`/`N`, `gn`, a mark) matches this ONCE, so no consumer can silently degrade a
/// subset of the operators into a bare move (#800).
#[derive(Clone, Copy, PartialEq, Eq)]
enum OpClass {
    /// `d` / `c` / `y`.
    Text(OpKind),
    /// `gu` / `gU` / `g~` / `g?`.
    Case(WordCase),
    /// `<` (`left`) / `>`.
    Shift { left: bool },
    /// `=`.
    Reindent,
    /// `gq` / `gw` (`keep`).
    Format { keep: bool },
    /// `gc`.
    Comment,
    /// `!` — resolved by the frontend into a pre-seeded `:{range}!` line, never an edit here.
    Filter,
}

impl Op {
    fn class(self) -> OpClass {
        match self {
            Op::Delete => OpClass::Text(OpKind::Delete),
            Op::Change => OpClass::Text(OpKind::Change),
            Op::Yank => OpClass::Text(OpKind::Yank),
            Op::CaseLower => OpClass::Case(WordCase::Downcase),
            Op::CaseUpper => OpClass::Case(WordCase::Upcase),
            Op::CaseToggle => OpClass::Case(WordCase::Toggle),
            Op::Rot13 => OpClass::Case(WordCase::Rot13),
            Op::ShiftRight => OpClass::Shift { left: false },
            Op::ShiftLeft => OpClass::Shift { left: true },
            Op::Reindent => OpClass::Reindent,
            Op::Format => OpClass::Format { keep: false },
            Op::FormatKeep => OpClass::Format { keep: true },
            Op::Comment => OpClass::Comment,
            Op::Filter => OpClass::Filter,
        }
    }
}

impl OpClass {
    /// The operator as a search / `gn` operator. `None` for the filter, which has no buffer-side form.
    fn search_op(self) -> Option<SearchOp> {
        Some(match self {
            OpClass::Text(OpKind::Delete) => SearchOp::Delete,
            OpClass::Text(OpKind::Change) => SearchOp::Change,
            OpClass::Text(OpKind::Yank) => SearchOp::Yank,
            OpClass::Case(c) => SearchOp::Case(c),
            OpClass::Shift { left } => SearchOp::Shift { left },
            OpClass::Reindent => SearchOp::Reindent,
            OpClass::Format { keep } => SearchOp::Format { keep },
            OpClass::Comment => SearchOp::Comment,
            OpClass::Filter => return None,
        })
    }

    /// The operator as a mark-motion operator. `None` for the filter.
    fn mark_op(self) -> Option<MarkOp> {
        Some(match self {
            OpClass::Text(OpKind::Delete) => MarkOp::Delete,
            OpClass::Text(OpKind::Change) => MarkOp::Change,
            OpClass::Text(OpKind::Yank) => MarkOp::Yank,
            OpClass::Case(c) => MarkOp::Case(c),
            OpClass::Shift { left } => MarkOp::Shift { left },
            OpClass::Reindent => MarkOp::Reindent,
            OpClass::Format { keep } => MarkOp::Format { keep },
            OpClass::Comment => MarkOp::Comment,
            OpClass::Filter => return None,
        })
    }
}

/// The operator-pending axis: an armed operator plus the RAW count typed before it (`0` = none, so `dG` and
/// `2dG` stay distinguishable — #791).
#[derive(Clone, Copy, PartialEq, Eq)]
struct OpPending {
    op: Op,
    count: u32,
}

/// Multiply two raw count segments (`3d2w` = 6), where `0` means "no count typed" (not a factor of zero),
/// clamped to [`MAX_COUNT`].
fn mul_raw(a: u32, b: u32) -> u32 {
    match (a, b) {
        (0, x) | (x, 0) => x,
        (a, b) => a.saturating_mul(b).min(MAX_COUNT),
    }
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
struct NormalState {
    /// Count axis: the accumulating numeric prefix for the next motion/operator.
    count: u32,
    /// The RAW count typed before a `"x` register prefix (`0` = none). A register prefix ends a count
    /// segment without consuming it: nvim multiplies the segments, `2"a3x` deletes 6 (#796).
    reg_count: u32,
    /// A `"x` register prefix has been emitted for the command being built. Not a count, but it makes the
    /// state non-pristine (the next key still belongs to the same command).
    register: bool,
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
        self.typed() == 0 && !self.register && !self.armed() && self.awaiting == Awaiting::Nothing
    }

    /// An operator is armed (or its forced wise, `dv`): the state [`InputEngine::pending`] reports as
    /// [`Pending::Operator`], and the one in which [`vim::continues_op`] decides whether a key continues or
    /// aborts the construct (#790).
    fn armed(&self) -> bool {
        self.op.is_some() || self.forced_wise.is_some()
    }

    /// The RAW effective count of the command being built: the product of every count segment typed so far
    /// — before a register prefix, before the operator, and after it (`2"a3d2l` = 12) — or `0` when no
    /// count was typed at all. This is the ONE count model every consumer reads (#789, #791, #796, #799).
    fn typed(&self) -> u32 {
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
    /// Insert-mode `CTRL-X` completion submode (`i_CTRL-X`): the next key selects a completion SOURCE. Only
    /// `CTRL-L` (whole-line, current buffer) is in scope this slice — the frontend intercepts it before
    /// `feed_insert` (it needs the buffer) and resolves the candidate lines. Any other key reaching the
    /// engine cancels the submode. A one-key expectation local to Insert; ALWAYS separate from the Emacs
    /// profile's `C-x` prefix (`emacs_prefix`), which lives on the non-modal `feed` path.
    ctrl_x: bool,
    /// Insert resumed after an `i_CTRL-O` one-shot ended the previous change (#795): the next edit opens
    /// a fresh `i` dot-repeat change. Vim's `arrow_used`/`stop_arrow` pair. Local to Insert.
    resumed: bool,
    /// `CTRL-G U` was typed: the next `<Left>`/`<Right>` stays inside the undo group and the `.` record
    /// ([`Command::InsertCaretStep`]). Any key consumes it. Local to Insert.
    keep_undo: bool,
    /// The char the previous Insert key typed (the printable-insert path), for `0 CTRL-D` / `^ CTRL-D`. Any
    /// other key clears it. Local to Insert.
    last_typed: Option<char>,
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

/// A command line set aside by [`InputEngine::suspend_cmdline`] (opaque; `None` when none was open).
pub struct SuspendedCmdline(Option<CmdLine>);

/// The Normal/Visual input state, held as three **orthogonal axes** — `count`, the operator-pending `op`,
/// and the one-shot `awaiting` key-expectation — plus sticky repeat state. `feed` resolves them in a fixed
/// precedence (mode → awaiting tier → base keys), so the hierarchy is explicit, not encoded in field order.
pub struct InputEngine {
    /// Which input profile is active — Vim (modal grammar) or Emacs (non-modal). Chosen at construction.
    input_profile: InputProfile,
    /// The error the last key failed with, when the engine alone can tell (`dp` outside diff mode is E99),
    /// for the frontend's status line. Taken with [`InputEngine::take_error`].
    error: Option<&'static str>,
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
    /// The `.` record ([`last_change`](Self::last_change), [`recording`](Self::recording)) as it was before
    /// the Normal-mode change last folded in by [`record`](Self::record), kept until the next command, so
    /// [`change_failed`](Self::change_failed) can put it back when that change fails (#1179).
    replaced_change: Option<(Option<ChangeIntent>, Option<ChangeIntent>)>,
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
    /// The expression of a `"=` register held with [`InputEngine::pending_record_register`] (`=`), for the
    /// change that follows (#808).
    pending_record_expr: Option<String>,
    /// The count typed before a PURE insert-entry (`3i`/`3o`/`3A`; VIM-CNT-INS), captured BEFORE
    /// [`action`](Self::action) resets the axes and handed to the [`ChangeIntent`] the next insert-entry
    /// opens. `0` between entries; only the six pure insert-entries set it (change-family entries leave it
    /// clear, so their count never repeats text). See [`InputEngine::insert_entry`].
    pending_insert_count: u32,
    /// The count-on-insert replication tail (VIM-CNT-INS) produced when a count-prefixed insert session
    /// closes on `<Esc>`: the `(count - 1)` extra repeats of the typed text plus the `EnterNormal` that
    /// leaves Insert. [`feed`](Self::feed) takes it and returns it as a [`Feed::Repeat`] in place of the
    /// bare `EnterNormal`, so the repeats apply as ONE undo group (consecutive edits coalesce). `None`
    /// for a count-less insert and for every non-insert command.
    pending_insert_replay: Option<Repeat>,
    /// The operator+count armed when `/` opened the search line, held across the minibuffer's pattern entry
    /// so `submit_search` can fold them into the finished command (`d/pat`, `2/pat`). `/` captures this
    /// BEFORE `reset()` wipes the axes; `None` between searches. See [`InputEngine::submit_search`].
    pending_search: Option<(SearchOp, u32, bool)>,
    /// The Insert layer's owned transient state (the `CTRL-G` prefix). Dropped when the active
    /// namespace is not Insert — KL-OBL-4.
    insert: InsertState,
    /// The activation stack (KL-OBL-5): return ADDRESSES for one-shot commands one layer borrows to
    /// run in another. `i_CTRL-O` pushes `Suspended{resume: Insert}`; the completing command pops it,
    /// resuming Insert. Empty in steady state. A stack rather than a bool so it records *whence* and
    /// nests for free (the general form; `i_CTRL-O` is its depth-1 case, `t_CTRL-\ CTRL-O` its second).
    activations: Vec<Suspended>,
    /// `v_CTRL-O` from Select: the next complete Visual command returns to Select if the selection is
    /// still live (nvim's `restart_VIsual_select`). Set by `CTRL-O` in Select, consumed by the command.
    select_shot: bool,
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
    /// Vim 'tildeop' (`:set top`): `~` is an operator (`~w`, `~~`) instead of switching `count` chars.
    tildeop: bool,
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
    /// A bare `ESC` was typed under the Emacs profile: ESC is the meta prefix (`esc-map`), so the NEXT key is
    /// read as Meta-modified whenever it arrives (`ESC u` = `M-u`, Emacs 31.1). This is what makes Meta
    /// work when the terminal's ESC and the key reach ruse in separate reads (#896).
    emacs_meta: bool,
    /// The Emacs profile's nine-tier keymap (F-012 / D-045). Built once; consulted only on the Emacs path.
    /// Present regardless of profile, mirroring `profile: VimProfile` — both are cheap to build.
    emacs: EmacsProfile,
    /// The Native profile's leader (which-key) tier is ARMED (F-013 NAT-2): `<leader>` (Space) was pressed
    /// from a clean Normal base, so the NEXT key resolves in the leader map rather than the Vim grammar.
    /// `false` in steady state and ALWAYS `false` under the Vim/Emacs profiles (only the Native+Normal path
    /// ever sets it). The depth-1 case of the discovery tier; nested groups (`<leader>g …`) layer on by tag.
    leader: bool,
    /// The command-line completion sources (#613): built-ins first, more via [`Self::completer_mut`].
    completer: complete::Completer,
}

/// A short human label for a key, for the palette's binding column (F-004 #2).
fn key_label(code: KeyCode) -> String {
    match code {
        KeyCode::Esc => "Esc".into(),
        KeyCode::Enter => "CR".into(),
        KeyCode::Backspace => "BS".into(),
        KeyCode::Tab => "Tab".into(),
        KeyCode::Char(' ') => "Space".into(),
        KeyCode::Char(c) => c.to_string(),
        other => format!("{other:?}"),
    }
}

/// One key as the palette shows it: `u`, `J`, `C-r`, `M-x`, `Esc`.
fn probe_label(key: KeyEvent) -> String {
    let base = key_label(key.code);
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        format!("C-{base}")
    } else if key.modifiers.contains(KeyModifiers::ALT) {
        format!("M-{base}")
    } else {
        base
    }
}

/// The keys [`InputEngine::binding_labels`] tries, simplest first: plain printable keys (an uppercase
/// letter with SHIFT, as a terminal reports it), the special keys, CTRL chords, then Meta chords.
fn probe_keys() -> Vec<KeyEvent> {
    let mut keys = Vec::new();
    for c in ' '..='~' {
        let m = if c.is_ascii_uppercase() {
            KeyModifiers::SHIFT
        } else {
            KeyModifiers::NONE
        };
        keys.push(KeyEvent::new(KeyCode::Char(c), m));
    }
    for code in [
        KeyCode::Esc,
        KeyCode::Enter,
        KeyCode::Backspace,
        KeyCode::Tab,
        KeyCode::Delete,
        KeyCode::Insert,
        KeyCode::Home,
        KeyCode::End,
        KeyCode::PageUp,
        KeyCode::PageDown,
        KeyCode::Left,
        KeyCode::Right,
        KeyCode::Up,
        KeyCode::Down,
    ] {
        keys.push(KeyEvent::new(code, KeyModifiers::NONE));
    }
    for c in ('a'..='z').chain(['/', '_', '@', ' ', ']', '\\', '^']) {
        keys.push(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL));
    }
    for c in 'a'..='z' {
        keys.push(KeyEvent::new(KeyCode::Char(c), KeyModifiers::ALT));
    }
    keys
}

/// Label every still-unlabelled entry of `commands` that `feed` (the result of typing `seq`) runs.
fn note_binding(out: &mut [Option<String>], commands: &[Command], feed: &Feed, seq: &[KeyEvent]) {
    let cmd = match feed {
        Feed::Cmd(c) => c,
        Feed::Replay(cs) if cs.len() == 1 => &cs[0],
        _ => return,
    };
    // `0` and `$` are the counted motions; the registry names the same moves without a count.
    let same = |want: &Command| {
        want == cmd
            || matches!(
                (want, cmd),
                (Command::MoveLineStart, Command::Move(1, Motion::LineStart))
                    | (Command::MoveLineEnd, Command::Move(1, Motion::LineEnd))
            )
    };
    for (slot, want) in out.iter_mut().zip(commands) {
        if slot.is_none() && same(want) {
            let labels: Vec<String> = seq.iter().map(|&k| probe_label(k)).collect();
            // `gJ`, `ZZ` read as one word; a chord in the sequence is spaced (`C-x C-s`, `C-g u`).
            *slot = Some(if labels.iter().all(|l| l.chars().count() == 1) {
                labels.concat()
            } else {
                labels.join(" ")
            });
        }
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
            error: None,
            profile: VimProfile::new(),
            normal: NormalState::default(),
            last_find: None,
            last_search: None,
            last_change: None,
            recording: None,
            replaced_change: None,
            last_inserted: Vec::new(),
            pending_record_register: None,
            pending_record_expr: None,
            pending_insert_count: 0,
            pending_insert_replay: None,
            pending_search: None,
            insert: InsertState::default(),
            activations: Vec::new(),
            select_shot: false,
            cmdline: None,
            cmdwin: None,
            ex_history: CmdHistory::new(history::DEFAULT_CAP),
            search_history: CmdHistory::new(history::DEFAULT_CAP),
            lang_map: HashMap::new(),
            lang_active: false,
            tildeop: false,
            emacs_arg: None,
            emacs_prefix: None,
            emacs_meta: false,
            emacs: EmacsProfile::new(),
            leader: false,
            completer: complete::Completer::default(),
        }
    }

    /// The error the last fed key failed with, if the engine raised one (nvim's message), cleared by the
    /// read.
    pub fn take_error(&mut self) -> Option<&'static str> {
        self.error.take()
    }

    /// Whether a one-shot command borrowed from another layer is in flight (`i_CTRL-O`): keys resolve
    /// through the Normal grammar until it completes and pops its return address (KL-OBL-5).
    pub fn in_one_shot(&self) -> bool {
        !self.activations.is_empty()
    }

    /// Whether Insert resumed after an `i_CTRL-O` one-shot that did not open a change of its own (so the
    /// next typed text is a new change, and a new undo step, #678); false after `<C-o>cw`, whose change
    /// owns the text typed next.
    pub fn insert_resumed(&self) -> bool {
        self.insert.resumed
    }

    /// Human labels for the keys that run each of `commands` in `mode` under this engine's profile (F-004
    /// #2 palette column, #956), `None` where no key of one or two strokes does. Found by feeding keys to a
    /// fresh engine of the same profile, so the label is what typing it does, grammar keys included (`u`,
    /// `J`, `0`, `CTRL-R`, Emacs `C-x C-s`), and not a table that can drift from the grammar. One-key
    /// bindings win over two-key ones; a count is never part of a binding.
    #[must_use]
    pub fn binding_labels(&self, commands: &[Command], mode: Mode) -> Vec<Option<String>> {
        let mut out: Vec<Option<String>> = vec![None; commands.len()];
        let keys = probe_keys();
        let profile = self.input_profile;
        let feed_seq = |seq: &[KeyEvent]| {
            let mut e = InputEngine::with_profile(profile);
            let mut last = Feed::Pending;
            for &k in seq {
                last = e.feed(k, mode);
            }
            last
        };
        let mut prefixes = Vec::new();
        for &k in &keys {
            let feed = feed_seq(&[k]);
            // A count digit is pending too, but a count is not part of a binding.
            let count = k.modifiers.is_empty() && matches!(k.code, KeyCode::Char('1'..='9'));
            if matches!(feed, Feed::Pending) && !count {
                prefixes.push(k);
            }
            note_binding(&mut out, commands, &feed, &[k]);
        }
        for &p in &prefixes {
            if out.iter().all(Option::is_some) {
                break;
            }
            for &k in &keys {
                let feed = feed_seq(&[p, k]);
                note_binding(&mut out, commands, &feed, &[p, k]);
            }
        }
        out
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
    /// [`Feed::CmdlineInsertUnder`] / [`Feed::CmdlineInsertLine`], after it has resolved the text from the
    /// buffer. No-op when no line is open; splicing also ends any history-recall walk.
    pub fn cmdline_splice(&mut self, text: &str) {
        if let Some(cl) = self.cmdline.as_mut() {
            cl.insert(text);
        }
    }

    /// Splice a register's text at the caret — the frontend's response to [`Feed::CmdlineInsertRegister`].
    /// As in nvim, a linewise register drops its final newline and every remaining newline is typed as a
    /// literal `^M` (`:<C-r>a` on `"l1\nl2\n"` gives `l1^Ml2`).
    pub fn cmdline_splice_register(&mut self, text: &str, linewise: bool) {
        let text = if linewise {
            text.strip_suffix('\n').unwrap_or(text)
        } else {
            text
        };
        self.cmdline_splice(&text.replace('\n', "\r"));
    }

    /// The completion sources, to register another ([`complete::Completer::register`]).
    pub fn completer_mut(&mut self) -> &mut complete::Completer {
        &mut self.completer
    }

    /// Run completion key `key` on the open `:` line — the frontend's answer to [`Feed::CmdlineComplete`],
    /// with `ctx` for what lies outside the line. nvim v0.12.6 defaults (`wildmode=full`,
    /// `wildoptions=pum`): one match is inserted; several open the wildmenu on the first (`<S-Tab>`: the
    /// last); none leaves the line as it is. Where nothing completes (`:s/a/`), `<Tab>` types a literal
    /// `^I`.
    pub fn cmdline_complete(&mut self, key: WildKey, ctx: &dyn complete::CompletionContext) {
        let Some(cl) = self.cmdline.as_mut() else {
            return;
        };
        let before = cl.before_cursor().to_string();
        let Some(c) = self.completer.complete(&before, ctx) else {
            if key == WildKey::Next {
                cl.insert("\t");
            }
            return;
        };
        let mut items = c.matches.items;
        let word = &before[c.start..];
        match key {
            WildKey::List => cl.listing = (!items.is_empty()).then_some(items),
            WildKey::Longest => {
                let longest = complete::longest_common(&items, c.files);
                if longest.len() > word.len() {
                    cl.replace_word(c.start, &longest);
                }
            }
            WildKey::All if !items.is_empty() => {
                let all: Vec<&str> = items.iter().map(|i| i.text.as_str()).collect();
                cl.replace_word(c.start, &all.join(" "));
            }
            WildKey::All => {}
            WildKey::Next | WildKey::Prev => {
                cl.listing = None;
                match items.len() {
                    0 => {}
                    1 => cl.replace_word(c.start, &items.remove(0).text),
                    n => {
                        let first = if key == WildKey::Next { 0 } else { n - 1 };
                        cl.start_wild(c.start, items, c.matches.pum_at, c.files, first);
                    }
                }
            }
        }
    }

    /// The open wildmenu for the popup menu: its entries, the selected one, and the char column of the
    /// line where the entries' text aligns. `None` when no wildmenu is open.
    #[must_use]
    pub fn cmdline_wild(&self) -> Option<(Vec<&str>, Option<usize>, usize)> {
        let w = self.cmdline.as_ref()?.wild.as_ref()?;
        Some((
            w.items.iter().map(|i| i.pum.as_str()).collect(),
            w.selected,
            w.pum_char_col(),
        ))
    }

    /// The matches `c_CTRL-D` listed on the open line, for the frontend to lay out in columns
    /// ([`complete::list_columns`]).
    #[must_use]
    pub fn cmdline_listing(&self) -> Option<&[complete::Item]> {
        self.cmdline.as_ref()?.listing.as_deref()
    }

    /// Keys on an open wildmenu (`:help wildmenu`): `<Tab>`/`CTRL-N`/`<Right>` and `<S-Tab>`/`CTRL-P`/
    /// `<Left>` cycle, `CTRL-E` restores the typed text, `CTRL-Y` keeps the match. `<Up>`/`<Down>` keep it
    /// too; on file names `<Down>` descends into a selected directory and `<Up>` completes the parent
    /// directory (nvim v0.12.6). `<Esc>` abandons the line as ever; any other key keeps the match and then
    /// acts. `None` when the key is not the wildmenu's to take.
    fn feed_wild(&mut self, key: KeyEvent) -> Option<Feed> {
        let cl = self.cmdline.as_mut()?;
        cl.wild.as_ref()?;
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        match key.code {
            KeyCode::BackTab => cl.wild_step(false),
            KeyCode::Tab => cl.wild_step(!shift),
            KeyCode::Right => cl.wild_step(true),
            KeyCode::Left => cl.wild_step(false),
            KeyCode::Char('n') if ctrl => cl.wild_step(true),
            KeyCode::Char('p') if ctrl => cl.wild_step(false),
            KeyCode::Char('e') if ctrl => cl.wild_cancel(),
            KeyCode::Char('y') if ctrl => cl.wild = None,
            KeyCode::PageUp | KeyCode::PageDown => cl.wild = None,
            KeyCode::Up | KeyCode::Down => {
                let files = cl.wild.as_ref().is_some_and(|w| w.files);
                let word = cl.wild_word().map(|(s, w)| (s, w.to_string()));
                cl.wild = None;
                let (Some((start, word)), true) = (word, files) else {
                    return Some(Feed::Pending);
                };
                if key.code == KeyCode::Down {
                    return Some(if word.ends_with('/') {
                        Feed::CmdlineComplete(WildKey::Next)
                    } else {
                        Feed::Pending
                    });
                }
                cl.replace_word(start, &complete::parent_word(&word));
                return Some(Feed::CmdlineComplete(WildKey::Next));
            }
            KeyCode::Esc => return None,
            _ => {
                cl.wild = None;
                return None;
            }
        }
        Some(Feed::Pending)
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

    /// Set aside the open command line (a `gQ` Ex-mode line that stays open while its command runs), so
    /// keys a `:normal` feeds reach Normal mode instead of the line (#807). [`Self::resume_cmdline`] puts
    /// it back.
    pub fn suspend_cmdline(&mut self) -> SuspendedCmdline {
        SuspendedCmdline(self.cmdline.take())
    }

    /// Restore a command line [`Self::suspend_cmdline`] set aside, replacing any line left open since.
    pub fn resume_cmdline(&mut self, saved: SuspendedCmdline) {
        self.cmdline = saved.0;
    }

    /// Open the command-line namespace with `prefix` (`:`/`/`), optionally as `gQ` Ex mode.
    fn open_cmdline(&mut self, prefix: char, ex_mode: bool) {
        self.cmdline = Some(CmdLine {
            ex_mode,
            // A search line follows the Insert language state; a `:` line never starts mapped (#810).
            lang: matches!(prefix, '/' | '?') && self.lang_active,
            ..CmdLine::new(prefix)
        });
    }

    /// Open the `:` command-line PRE-SEEDED with `seed` and the caret at its end — the landing point of the
    /// Normal-mode `!{motion}` / `!!` filter operator. The frontend resolves the motion's line range and
    /// seeds `"{first},{last}!"`, so typing the shell command and pressing `<CR>` runs the SAME
    /// `:{range}!{cmd}` ex filter (reusing its executor + `shell.rs`). Vim shows this pre-filled `:.,.+N!`.
    pub fn open_filter_cmdline(&mut self, seed: &str) {
        let mut cl = CmdLine::new(':');
        cl.set_text(seed.to_string());
        self.cmdline = Some(cl);
    }

    /// Open the command-line window (`:help cmdwin`) for `kind` — `:` mirrors the ex ring, `/`/`?` the
    /// shared search ring. The frontend calls this when `q:`/`q/`/`q?` is recognised (the macro layer routes
    /// the `q`-prefixed `:`/`/`/`?` here instead of arming a macro recording). No-op if already open.
    pub fn open_cmdwin(&mut self, kind: char) {
        // The window opens from a clean Normal base: no operator/count from an earlier search line applies.
        self.pending_search = None;
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
            mx: true,
            ..CmdLine::new(':')
        });
    }

    /// Open the expression-register prompt (`"=` / `<C-r>=`, `:help quote=`): the same command-line namespace,
    /// reading an EXPRESSION. The `=` prefix renders the prompt glyph. On `<CR>` the collected string is handed
    /// to the evaluator via the command `target` produces (see [`ExprTarget`]); `<Esc>` aborts.
    fn open_expr_prompt(&mut self, target: ExprTarget) {
        self.cmdline = Some(CmdLine {
            expr: Some(target),
            ..CmdLine::new('=')
        });
    }

    /// Route a key into the open command-line buffer (F-026). The namespace owns the buffer and its caret
    /// (`:help cmdline-editing`): a printable key types at the caret, `<Left>`/`<Right>`/`<Home>`/`<End>`
    /// and the word motions move it, `<BS>`/`<Del>`/`CTRL-U`/`CTRL-W` delete around it, `<Esc>` aborts, and
    /// `<CR>` finalises — a search folds through [`Self::submit_search`], an ex line becomes
    /// [`Feed::ExecuteEx`]. In `gQ` Ex mode the line re-opens after `<CR>` until `:visual`/`:vi`/an empty
    /// line exits.
    fn feed_cmdline(&mut self, mut key: KeyEvent) -> Feed {
        // `c_CTRL-V`/`c_CTRL-Q`: the collector owns the key; a code ended by a non-digit hands that key
        // back to be processed as ordinary input.
        if let Some(cl) = self.cmdline.as_mut().filter(|cl| cl.literal.is_some()) {
            match cl.feed_literal(key) {
                cmdline::LiteralStep::Reprocess(k) => key = k,
                _ => return Feed::Pending,
            }
        }
        let Some(cl) = self.cmdline.as_mut() else {
            return Feed::Ignored;
        };
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        // `c_CTRL-K {char1} {char2}`: the digraph collector owns the next two keys.
        if cl.digraph.is_some() {
            cl.feed_digraph(key);
            return Feed::Pending;
        }
        // `c_CTRL-R` prefix armed: THIS key selects what to splice at the caret. Checked before the
        // history-recall match below so that `C-r C-p` is consumed as a selector, not as `<C-p>` recall.
        // `CTRL-W`/`CTRL-A`/`CTRL-L` name the word/WORD/line under the cursor; `CTRL-R`/`CTRL-O` (insert
        // literally) keep the prefix armed for the register name; a plain char names a register, which the
        // frontend reads (the engine has none). Any other key aborts the prefix and stays on the line.
        if cl.ctrl_r {
            cl.ctrl_r = false;
            return match key.code {
                KeyCode::Char('w') if ctrl => Feed::CmdlineInsertUnder { big: false },
                KeyCode::Char('a') if ctrl => Feed::CmdlineInsertUnder { big: true },
                KeyCode::Char('l') if ctrl => Feed::CmdlineInsertLine,
                KeyCode::Char('r' | 'o') if ctrl => {
                    cl.ctrl_r = true;
                    Feed::Pending
                }
                // `C-r =` (the expression register's prompt) is not wired on the command line yet.
                KeyCode::Char('=') => Feed::Pending,
                KeyCode::Char(c) if !is_chord(key) => Feed::CmdlineInsertRegister(c),
                _ => Feed::Pending,
            };
        }
        // The wildmenu takes its keys first (`CTRL-N`/`CTRL-P` cycle there instead of recalling history).
        if cl.wild.is_some() {
            if let Some(feed) = self.feed_wild(key) {
                return feed;
            }
        }
        let Some(cl) = self.cmdline.as_mut() else {
            return Feed::Ignored;
        };
        // Completion keys on the `:` line (`:help cmdline-completion`); the `/` line completes nothing
        // (nvim v0.12.6 neither inserts nor completes a `<Tab>` there).
        if cl.prefix == ':' && !cl.mx && cl.expr.is_none() {
            let wild_key = match key.code {
                KeyCode::BackTab => Some(WildKey::Prev),
                KeyCode::Tab if key.modifiers.contains(KeyModifiers::SHIFT) => Some(WildKey::Prev),
                KeyCode::Tab => Some(WildKey::Next),
                KeyCode::Char('i') if ctrl => Some(WildKey::Next),
                KeyCode::Char('d') if ctrl => Some(WildKey::List),
                KeyCode::Char('l') if ctrl => Some(WildKey::Longest),
                KeyCode::Char('a') if ctrl => Some(WildKey::All),
                _ => None,
            };
            if let Some(k) = wild_key {
                return Feed::CmdlineComplete(k);
            }
        }
        if ctrl && key.code == KeyCode::Char('r') && !cl.mx {
            cl.ctrl_r = true;
            return Feed::Pending;
        }
        // History recall (`:help cmdline-history`). nvim-VERIFIED distinction: `<Up>`/`<Down>` recall
        // PREFIX-FILTERED by the text before the caret; `<C-p>`/`<C-n>`, `<S-Up>`/`<S-Down>` and
        // `<PageUp>`/`<PageDown>` walk the RAW ring unfiltered.
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        match key.code {
            KeyCode::Up if !shift => return self.cmdline_recall(true, true),
            KeyCode::Down if !shift => return self.cmdline_recall(false, true),
            KeyCode::Up | KeyCode::PageUp => return self.cmdline_recall(true, false),
            KeyCode::Down | KeyCode::PageDown => return self.cmdline_recall(false, false),
            KeyCode::Char('p') if ctrl => return self.cmdline_recall(true, false),
            KeyCode::Char('n') if ctrl => return self.cmdline_recall(false, false),
            _ => {}
        }
        let Some(cl) = self.cmdline.as_mut() else {
            return Feed::Ignored;
        };
        // CTRL aliases (nvim v0.12.6): `c_CTRL-C` and `<C-[>` abandon the line like `<Esc>`, `c_CTRL-H` is
        // `<BS>`, `c_CTRL-J`/`<C-m>` run it like `<CR>`, `c_CTRL-B`/`c_CTRL-E` are `<Home>`/`<End>`. The
        // Emacs `M-x` minibuffer shares this editor with Emacs keys instead: `C-g` quits, `C-a`/`C-e`/`C-b`/
        // `C-f` move, `C-d` deletes forward, `C-k` kills to the end; `C-c`/`C-h`/`C-u`/`C-w` are inert.
        // `c_CTRL-^` flips the language map for this line; on a `/`/`?` line it is the Insert state too
        // (`'imsearch'` -1), so the next search and Insert keep it (nvim `command_line_toggle_langmap`, #810).
        if ctrl && !cl.mx && matches!(key.code, KeyCode::Char('^' | '6')) {
            cl.lang = !cl.lang;
            if matches!(cl.prefix, '/' | '?') && cl.expr.is_none() {
                self.lang_active = cl.lang;
            }
            return Feed::Pending;
        }
        key = match key.code {
            KeyCode::Char('[') if ctrl => plain(KeyCode::Esc),
            KeyCode::Char('c') if ctrl && !cl.mx => plain(KeyCode::Esc),
            KeyCode::Char('g') if ctrl && cl.mx => plain(KeyCode::Esc),
            KeyCode::Char('h') if ctrl && !cl.mx => plain(KeyCode::Backspace),
            KeyCode::Char('j' | 'm') if ctrl => plain(KeyCode::Enter),
            KeyCode::Char('b') if ctrl && !cl.mx => plain(KeyCode::Home),
            KeyCode::Char('e') if ctrl => plain(KeyCode::End),
            KeyCode::Char('a') if ctrl && cl.mx => plain(KeyCode::Home),
            KeyCode::Char('b') if ctrl && cl.mx => plain(KeyCode::Left),
            KeyCode::Char('f') if ctrl && cl.mx => plain(KeyCode::Right),
            _ => key,
        };
        // `<S-Left>`/`<C-Left>` and `<S-Right>`/`<C-Right>` move by space-separated WORD.
        let word = key
            .modifiers
            .intersects(KeyModifiers::SHIFT | KeyModifiers::CONTROL);
        // An empty Vim line (not `M-x`, not `gQ` Ex mode) is abandoned by `<BS>`/`<Del>` (#811).
        let cancels_empty = !cl.mx && !cl.ex_mode;
        match key.code {
            KeyCode::Esc => {
                self.cmdline = None;
                // Abandoning a `/` line abandons the operator/count it carried (`d/<Esc>`): left behind, a
                // later `q/` window search would fold into it and delete (#798).
                self.pending_search = None;
                Feed::Ignored
            }
            KeyCode::Backspace => {
                if cl.backspace() && cancels_empty {
                    return self.feed_cmdline(plain(KeyCode::Esc));
                }
                Feed::Pending
            }
            KeyCode::Delete => {
                if cl.delete() && cancels_empty {
                    return self.feed_cmdline(plain(KeyCode::Esc));
                }
                Feed::Pending
            }
            KeyCode::Char('d') if ctrl && cl.mx => {
                if cl.cursor < cl.buffer.chars().count() {
                    cl.delete();
                }
                Feed::Pending
            }
            KeyCode::Char('k') if ctrl && cl.mx => {
                cl.kill_after();
                Feed::Pending
            }
            KeyCode::Left if word => {
                cl.word_left();
                Feed::Pending
            }
            KeyCode::Right if word => {
                cl.word_right();
                Feed::Pending
            }
            KeyCode::Left => {
                cl.left();
                Feed::Pending
            }
            KeyCode::Right => {
                cl.right();
                Feed::Pending
            }
            KeyCode::Home => {
                cl.home();
                Feed::Pending
            }
            KeyCode::End => {
                cl.end();
                Feed::Pending
            }
            // `c_<Insert>` toggles overstrike.
            KeyCode::Insert => {
                cl.overstrike = !cl.overstrike;
                Feed::Pending
            }
            // `c_CTRL-U` — delete everything before the caret.
            KeyCode::Char('u') if ctrl && !cl.mx => {
                cl.kill_before();
                Feed::Pending
            }
            // `c_CTRL-W` — delete the word before the caret.
            KeyCode::Char('w') if ctrl && !cl.mx => {
                cl.kill_word_before();
                Feed::Pending
            }
            // `c_CTRL-V` / `c_CTRL-Q` — insert the next key literally (`<C-v><CR>` types a `^M`), or a
            // numeric code.
            KeyCode::Char('v' | 'q') if ctrl => {
                cl.literal = Some(cmdline::Literal::AwaitFirst);
                Feed::Pending
            }
            // `c_CTRL-K {char1} {char2}` — a digraph (Emacs `C-k` is kill-line, above).
            KeyCode::Char('k') if ctrl => {
                cl.digraph = Some(None);
                Feed::Pending
            }
            // Any other chord is unbound here and never appends its letter (#599). nvim appends some of them
            // (`<C-g>`, `<C-o>`, `<C-t>`, …) as a raw control character; ruse appends nothing.
            KeyCode::Char(_) if is_chord(key) => Feed::Pending,
            KeyCode::Char(c) => {
                cl.insert(c.encode_utf8(&mut [0; 4]));
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
                    // push). gQ stays in the SAME CmdLine, so reset its caret and walk for the next line.
                    self.ex_history.push(&text);
                    cl.cursor = 0;
                    cl.overstrike = false;
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
    /// (`<Up>`/`<C-p>`), else more-recent (`<Down>`/`<C-n>`); `filter` prefix-filters by the text before
    /// the caret (`<Up>`/`<Down>`) vs the raw ring (`<C-p>`/`<C-n>`). Chooses the ex vs search ring by
    /// prefix; the `M-x`/expression prompts have no history and pass the key through. Rewrites the buffer
    /// and puts the caret at its end.
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
        let current = cl.before_cursor().to_string();
        let recalled = if prev {
            ring.recall_prev(&mut cl.walk, &current, filter)
        } else {
            ring.recall_next(&mut cl.walk, &current, filter)
        };
        if let Some(text) = recalled {
            cl.set_text(text);
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
        let had_register = self.normal.register;
        let feed = self.unmatched_policy(ns, key);
        self.drop_aborted_register(had_register, feed)
    }

    /// An abort also abandons a `"x` prefix already handed to the core (`"b<Esc>p` pastes the UNNAMED
    /// register in nvim): clear the core's pending register in place of the bare `Ignored`.
    fn drop_aborted_register(&self, had_register: bool, feed: Feed) -> Feed {
        match feed {
            Feed::Ignored if had_register => Feed::Cmd(Command::SetRegister(None)),
            Feed::Beep { .. } => Feed::Beep {
                clear_register: had_register,
            },
            feed => feed,
        }
    }

    fn unmatched_policy(&mut self, ns: Ns, key: KeyEvent) -> Feed {
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
                    // An unbound chord inserts nothing, never its letter (#599).
                    KeyCode::Char(c) if !is_chord(key) => Feed::Cmd(Command::InsertChar(c)),
                    // `open/insert` is about PRINTABLE keys; a non-printable unmatched key does
                    // nothing, which is not the same statement as `closed/ignore`.
                    _ => Feed::Ignored,
                }
            }
            UnmatchedKey::Ignore | UnmatchedKey::Abort => {
                self.reset();
                // An unknown key in Normal, Visual or after an operator beeps in Vim (`nv_error` →
                // `clearopbeep`), which flushes the keys not typed. `<Esc>` beeps without flushing
                // (`nv_esc`), and a bare modifier or lock key is no key at all (#1111). An unbound chord
                // stays a plain no-op: several are nvim commands ruse resolves elsewhere or lacks (Visual
                // `<C-d>` scrolls in nvim), and failing them would stop a macro nvim runs on.
                let no_key = is_chord(key)
                    || matches!(
                        key.code,
                        KeyCode::Esc
                            | KeyCode::Modifier(_)
                            | KeyCode::Null
                            | KeyCode::CapsLock
                            | KeyCode::NumLock
                            | KeyCode::ScrollLock
                            | KeyCode::Media(_)
                    );
                if no_key {
                    Feed::Ignored
                } else {
                    Feed::Beep {
                        clear_register: false,
                    }
                }
            }
            UnmatchedKey::ReplaceSelection => {
                self.reset();
                match key.code {
                    // Vim Select: a printable key deletes the selection, inserts the char, enters Insert.
                    // The core (`Command::ReplaceSelection`) performs all three as one edit.
                    KeyCode::Char(c) if !is_chord(key) => Feed::Cmd(Command::ReplaceSelection(c)),
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
        // `!/pat` keeps the filter through the minibuffer: `submit_search` hands the span to the frontend.
        self.pending_search = Some((self.search_op(), self.mcount(), self.filter_armed()));
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
        let (op, count, filter) = self
            .pending_search
            .take()
            .unwrap_or((SearchOp::Move, 1, false));
        // Split the raw line into `{pattern}` + `{offset}` on the LAST unescaped delimiter (`/` forward,
        // `?` backward) — a `\/` in the pattern is literal (`:help search-offset`). `off` is `None` when
        // no offset part is present (`/foo`, `/foo/`), which distinguishes "reuse the last offset" from
        // "explicitly no offset" for the empty-pattern repeat below.
        let delim = if backward { '?' } else { '/' };
        let (raw_pattern, off) = split_search_offset(&line, delim);
        // Empty pattern → reuse the last search's PATTERN (Vim: `/<CR>` / `?<CR>` repeat it), adopting THIS
        // line's direction. A new offset on the empty line replaces the stored one; no offset part reuses
        // it (so `//e` re-searches the last pattern with `e`, and `//` repeats pattern AND offset).
        // With no previous pattern the search still runs, with an EMPTY pattern the core fails (E35, #1173).
        let (pattern, offset) = if raw_pattern.is_empty() {
            match &self.last_search {
                Some((p, _, last_off)) => (p.clone(), off.unwrap_or(*last_off)),
                None => (String::new(), SearchOffset::None),
            }
        } else {
            (raw_pattern, off.unwrap_or(SearchOffset::None))
        };
        if !pattern.is_empty() {
            self.last_search = Some((pattern.clone(), !backward, offset));
        }
        let cmd = Command::Search {
            op,
            count,
            pattern,
            backward,
            offset,
        };
        if filter {
            Feed::FilterSpan(cmd)
        } else {
            Feed::Cmd(cmd)
        }
    }

    /// `n` (`opposite = false`) / `N` (`opposite = true`): repeat the last search, REAPPLYING its offset.
    /// `n` keeps the stored direction, `N` flips it. Routed through [`Command::Search`] (carrying the
    /// offset) so the offset-aware match rule advances correctly even when the offset landed the cursor
    /// before the match. No prior search → a search with an EMPTY pattern, which the core fails and the
    /// frontend reports as E35: it reaches the core so the wanted column resets and a replay sees it, as nvim's
    /// `normal_search` sets `w_set_curswant` before it fails (#1173).
    fn repeat_search(&mut self, opposite: bool) -> Feed {
        let (pattern, forward, offset) =
            self.last_search
                .clone()
                .unwrap_or((String::new(), true, SearchOffset::None));
        let backward = if opposite { forward } else { !forward };
        // `n`/`N` are motions: `3n` steps three matches and `dn`/`cn`/`gUn` operate up to the match
        // (#799), through the same operator and count model as `/`.
        self.span_action(Command::Search {
            op: self.search_op(),
            count: self.mcount(),
            pattern,
            backward,
            offset,
        })
    }

    /// Record `pattern` as the last search (with its `forward` direction) so `n`/`N` repeat it. Used by the
    /// frontend after it resolves a `*`/`#` (word-under-cursor) search, whose pattern is known only once the
    /// buffer is read: `*` is forward, `#` backward. These carry no offset ([`SearchOffset::None`]).
    pub fn set_last_search(&mut self, pattern: String, forward: bool) {
        self.last_search = Some((pattern, forward, SearchOffset::None));
    }

    /// Replace the last search PATTERN only, keeping its direction and offset (forward with no offset when
    /// there was none), as an Ex `:s/pat/` or `:g/pat/` does in Vim (`save_re_pat`): `n` then finds `pat`.
    pub fn set_last_search_pattern(&mut self, pattern: String) {
        let (forward, offset) = self
            .last_search
            .as_ref()
            .map_or((true, SearchOffset::None), |&(_, f, o)| (f, o));
        self.last_search = Some((pattern, forward, offset));
    }

    /// The whole last-search state (pattern, direction, offset), for `:keeppatterns` to put back (#609).
    pub(crate) fn search_state(&self) -> Option<(String, bool, SearchOffset)> {
        self.last_search.clone()
    }

    /// Put back a state [`InputEngine::search_state`] returned.
    pub(crate) fn restore_search_state(&mut self, state: Option<(String, bool, SearchOffset)>) {
        self.last_search = state;
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
                Command::InsertPaste(t) | Command::ReplacePaste(t) => out.push_str(t),
                Command::DeleteBack => {
                    // `<BS>` deleted a whole cluster (#909), so the record drops one too.
                    let keep = unicode_segmentation::UnicodeSegmentation::grapheme_indices(
                        out.as_str(),
                        true,
                    )
                    .next_back()
                    .map_or(0, |(i, _)| i);
                    out.truncate(keep);
                }
                _ => {}
            }
        }
        Some(out)
    }

    /// A bracketed paste (`:help paste`, #600) in core mode `mode`. The text is never read as keys: no
    /// mapping, count, operator or abbreviation applies, and a pending prefix (`CTRL-R`, `CTRL-V`, a count,
    /// an operator, an Emacs `C-x` or `ESC`) is dropped. `text` has `\n` line endings ([`paste_text`]).
    /// Per nvim v0.12.6's `vim.paste`:
    /// - command line (and the Emacs minibuffer): the first line is inserted literally;
    /// - Insert: the text is inserted at the caret, part of the insert session (one undo step, `.`);
    /// - Replace: the text overwrites ([`Command::ReplacePaste`]);
    /// - Normal: `a{text}<Esc>`, recorded as that change for `.`;
    /// - Visual/Select: the selection is replaced by the text (Select's replace-selection path, #1073)
    ///   and Normal resumes. A block is deleted and the text goes at its top-left corner; the engine has
    ///   no buffer, so `visual_append` (the caller's answer to "does deleting the block leave the caret
    ///   past the end of its line") picks `a` over `i`. `.` repeats it as the Normal paste.
    ///
    /// The command-line window ignores a paste.
    pub fn paste(&mut self, text: &str, mode: Mode, visual_append: bool) -> Feed {
        if self.cmdline.is_some() {
            let line = text.split('\n').next().unwrap_or("");
            self.cmdline_splice(line);
            return Feed::Pending;
        }
        if self.input_profile == InputProfile::Emacs {
            self.emacs_meta = false;
            self.emacs_prefix = None;
            self.emacs_arg = None;
            return Feed::Cmd(Command::EmacsPaste(text.to_string()));
        }
        if self.cmdwin.is_some() {
            return Feed::Ignored;
        }
        let paste = Command::InsertPaste(text.to_string());
        match mode {
            Mode::Insert | Mode::Replace | Mode::VirtualReplace => {
                self.insert.ctrl_g = false;
                self.insert.ctrl_r = false;
                self.insert.ctrl_x = false;
                self.insert.digraph = None;
                self.insert.literal = None;
                self.insert.completion = None;
                // nvim drops a paste in Virtual Replace (`vim.paste` has no `Rv` branch); overwriting like
                // `R` keeps the text instead of losing it.
                let out = Feed::Cmd(if mode == Mode::Insert {
                    paste
                } else {
                    Command::ReplacePaste(text.to_string())
                });
                self.record(&out, mode);
                out
            }
            Mode::Normal | Mode::Visual { .. } | Mode::Select { .. } => {
                self.reset();
                self.leader = false;
                self.pending_record_register = None;
                self.pending_record_expr = None;
                let change = ChangeIntent {
                    lead: Command::EnterInsertAfter,
                    insert: vec![paste.clone(), Command::EnterNormal],
                    register: None,
                    expr: None,
                    entry_count: 1,
                };
                self.last_inserted = change.insert_body().to_vec();
                self.last_change = Some(change);
                let block = matches!(
                    mode,
                    Mode::Visual {
                        kind: SelectKind::Blockwise
                    } | Mode::Select {
                        kind: SelectKind::Blockwise
                    }
                );
                let cmds = match mode {
                    Mode::Normal => vec![Command::EnterInsertAfter, paste, Command::EnterNormal],
                    // A block is deleted, then the text goes at its top-left corner (not down every row).
                    _ if block => vec![
                        Command::DeleteSelection,
                        if visual_append {
                            Command::EnterInsertAfter
                        } else {
                            Command::EnterInsert
                        },
                        paste,
                        Command::EnterNormal,
                    ],
                    // Charwise/linewise: Select's replace-selection path (the core's `InsertPaste`
                    // over a selection), then Normal.
                    _ => vec![paste, Command::EnterNormal],
                };
                Feed::Replay(cmds)
            }
            // A terminal buffer's paste goes to its child; the session routes it before the engine.
            Mode::Terminal | Mode::TerminalNormal => Feed::Ignored,
        }
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
        let n = self.normal.typed();
        self.reset();
        n
    }

    /// The effective count, defaulting to 1: every typed count segment multiplied ([`NormalState::typed`]).
    fn mcount(&self) -> u32 {
        self.normal.typed().max(1)
    }

    /// The armed operator as a search / `*` / `gn` operator: [`SearchOp::Move`] with none armed. The filter
    /// has no core form; it yields the non-mutating `Yank` its [`Feed::FilterSpan`] carries.
    fn search_op(&self) -> SearchOp {
        match self.normal.op {
            Some(p) => p.op.class().search_op().unwrap_or(SearchOp::Yank),
            None => SearchOp::Move,
        }
    }

    /// Whether the armed operator is the `!` filter.
    fn filter_armed(&self) -> bool {
        self.normal
            .op
            .is_some_and(|p| p.op.class() == OpClass::Filter)
    }

    /// Emit a span command (a search, `n`/`N`, `*`/`#`, `gn`, a mark): applied as usual, or under `!` handed
    /// to the frontend as a [`Feed::FilterSpan`] to number its lines (#800).
    fn span_action(&mut self, cmd: Command) -> Feed {
        if self.filter_armed() {
            self.reset();
            Feed::FilterSpan(cmd)
        } else {
            self.action(cmd)
        }
    }

    /// Emit `m` — an operator command if one is armed, else a bare move — then clear the transient state.
    /// The repeat count is the operator count times the motion count (`3d2w` = 6), or the motion count for
    /// a bare move ([`mcount`](Self::mcount) folds every segment).
    fn motion(&mut self, m: Motion) -> Feed {
        let total = self.mcount();
        self.op_over_motion(m, total)
    }

    /// Compose the pending operator (if any) over motion `m` with an explicit `total`, then clear the
    /// transient state. `total` is the repeat count for most motions, but the ABSOLUTE 1-based TARGET LINE
    /// for [`Motion::GotoLine`] (whose operator range is `[min,max]` of the cursor and target lines). With
    /// no operator armed it is the bare `Move(total, m)`. Shared by [`motion`](Self::motion) (computed
    /// `total`) and [`screen_op`](Self::screen_op) (the frontend-resolved `H`/`M`/`L` target line).
    fn op_over_motion(&mut self, m: Motion, total: u32) -> Feed {
        let feed = match self.normal.op.map(|p| p.op.class()) {
            None => Feed::Cmd(Command::Move(total, m)),
            // `!{motion}` — the filter does not resolve to an edit here: hand the motion + folded count to the
            // frontend (it numbers the lines and opens the `:{range}!` cmdline).
            Some(OpClass::Filter) => Feed::FilterMotion {
                count: total,
                motion: m,
            },
            // `gu`/`gU`/`g~`/`g?` recase the operator span (no forced-wise / `cw`->`ce` rewrites apply).
            Some(OpClass::Case(case)) => Feed::Cmd(Command::CaseMotion {
                count: total,
                motion: m,
                case,
            }),
            // `>`/`<`, `=`, `gq`/`gw` over a motion — the planner works on the motion's LINES.
            Some(OpClass::Shift { left }) => Feed::Cmd(Command::ShiftMotion {
                left,
                count: total,
                motion: m,
            }),
            Some(OpClass::Reindent) => Feed::Cmd(Command::Reindent {
                count: total,
                motion: m,
            }),
            Some(OpClass::Format { keep }) => Feed::Cmd(Command::Format {
                count: total,
                motion: m,
                keep_cursor: keep,
            }),
            Some(OpClass::Comment) => Feed::Cmd(Command::Comment {
                count: total,
                motion: m,
            }),
            Some(OpClass::Text(op)) => Feed::Cmd(match (self.normal.forced_wise, op) {
                (Some(wise), op) => Command::OpForced {
                    op,
                    count: total,
                    motion: m,
                    wise,
                },
                (None, OpKind::Delete) => Command::Delete(total, m),
                // Vim `cw`/`cW` do not eat the trailing space and, unlike `ce`, changing a word's LAST char
                // changes only that char. The core's `change_range` applies that special case for
                // `WordFwd`/`BigWordFwd`, so pass the motion through unchanged (rewriting to `WordEnd` here
                // would wrongly jump into the next word from a word-final cursor).
                (None, OpKind::Change) => Command::Change(total, m),
                (None, OpKind::Yank) => Command::Yank(total, m),
            }),
        };
        self.reset();
        feed
    }

    /// The EFFECTIVE count for a viewport screen-motion (`H`/`M`/`L`), WITHOUT consuming any state. When an
    /// operator is armed this is the operator count times the motion count (`3d2H` = 6H → the 6th line from
    /// the top, verified against nvim), the SAME multiplication [`motion`](Self::motion) applies; with no
    /// operator it is the raw pending count (0 = none, which the viewport resolver treats as 1). A frontend
    /// intercept resolves its target line from this, then hands the line to [`screen_op`](Self::screen_op).
    pub fn screen_count(&self) -> u32 {
        if self.normal.op.is_some() {
            self.mcount()
        } else {
            self.normal.typed()
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

    /// What the engine is waiting for, WITHOUT consuming anything: the one query every session intercept
    /// and the macro layer consult before taking a key (#679, #680). A key expectation wins over an armed
    /// operator (`df` waits for the find char, not a motion). A bare count is still `Base`: nvim's `2q`
    /// stops a recording and `5zz` is a scroll.
    #[must_use]
    pub fn pending(&self) -> Pending {
        if self.cmdline.is_some()
            || self.cmdwin.is_some()
            || self.leader
            || self.emacs_prefix.is_some()
            || self.emacs_meta
        {
            return Pending::Argument;
        }
        match self.normal.awaiting {
            Awaiting::Nothing if self.normal.armed() => Pending::Operator,
            Awaiting::Nothing => Pending::Base,
            Awaiting::BracketPrefix { open_bracket } => Pending::Bracket { open: open_bracket },
            _ => Pending::Argument,
        }
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

    /// Hand a screen-line motion to the frontend ([`Feed::ScreenMotion`]), keeping the operator armed.
    fn screen_motion(&mut self, motion: ScreenMotion) -> Feed {
        Feed::ScreenMotion {
            motion,
            count: self.mcount(),
        }
    }

    /// Finish a [`Feed::ScreenMotion`] the frontend resolved to buffer byte `byte`: the bare move, or the
    /// armed operator over it as an exclusive charwise motion (`go`'s rules: `dgj` from a line's start is
    /// linewise). An inclusive one (`g$`) reaches here as the byte after its char.
    pub fn screen_motion_to(&mut self, byte: usize) -> Feed {
        let target = u32::try_from(byte.saturating_add(1)).unwrap_or(u32::MAX);
        self.op_over_motion(Motion::GotoByte, target)
    }

    /// Finish a [`Feed::ScreenMotion`] that kept the end-of-line wanted column (`gj` after `$`) and landed
    /// on the end of a line `lines - 1` below as `{lines}$`: the same place, inclusive, and the column
    /// stays at the end as nvim keeps it.
    pub fn screen_motion_line_end(&mut self, lines: u32) -> Feed {
        self.op_over_motion(Motion::LineEnd, lines.max(1))
    }

    /// Finish a [`Feed::ScreenMotion`] as `j` / `k` with its count: `gj` / `gk` without 'wrap'.
    pub fn screen_motion_lines(&mut self, down: bool) -> Feed {
        let total = self.mcount();
        self.op_over_motion(if down { Motion::Down } else { Motion::Up }, total)
    }

    /// Abandon a [`Feed::ScreenMotion`] that cannot move (`gj` on the last screen row): nvim beeps and
    /// drops the operator.
    pub fn screen_motion_fail(&mut self) -> Feed {
        let had_register = self.normal.register;
        self.reset();
        self.drop_aborted_register(
            had_register,
            Feed::Beep {
                clear_register: false,
            },
        )
    }

    /// Whether the Insert namespace is in a PLAIN-TEXT context right now: not mid a `CTRL-G`/`CTRL-R`
    /// prefix, a `CTRL-K` digraph, a `CTRL-V` literal entry, nor an `i_CTRL-O` one-shot. The frontend
    /// checks this before handling `i_CTRL-E`/`i_CTRL-Y`, so those never steal the key a pending prefix
    /// is waiting for (e.g. a `CTRL-R` register name).
    #[must_use]
    pub fn insert_plain_text_ctx(&self) -> bool {
        !self.insert.ctrl_g
            && !self.insert.ctrl_r
            && !self.insert.ctrl_x
            && self.insert.digraph.is_none()
            && self.insert.literal.is_none()
            && !self.in_one_shot()
    }

    /// Whether the Insert `CTRL-X` completion submode (`i_CTRL-X`) is armed — the previous key was `CTRL-X`
    /// and the engine awaits the source selector. The frontend checks this so `i_CTRL-X CTRL-L` (whole-line
    /// completion) is resolved against the buffer here rather than reaching `feed_insert`.
    #[must_use]
    pub fn insert_ctrl_x_pending(&self) -> bool {
        self.insert.ctrl_x
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

    /// `i_CTRL-E` / `i_CTRL-Y` while a completion cycle is active (`:help complete_CTRL-E`, nvim
    /// `ins_compl_stop`, #805): either ends the cycle. `CTRL-E` (`cancel`) puts the original text back and
    /// inserts nothing. `CTRL-Y` accepts the selected text; with more than one candidate the popup menu is
    /// showing (`'completeopt'` "menu") and it inserts nothing, with one there is no menu and the key goes
    /// on to insert the char above. `Some` is the consumed key's result; `None` means the caller runs the
    /// key as usual (no cycle was active, or a menu-less `CTRL-Y`).
    pub fn complete_end(&mut self, cancel: bool) -> Option<Feed> {
        let c = self.insert.completion.take()?;
        if cancel {
            let out = Feed::Cmd(Command::CompleteWord {
                back: c.applied,
                text: c.base,
            });
            self.record(&out, Mode::Insert);
            return Some(out);
        }
        (c.cands.len() > 1).then_some(Feed::Pending)
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

    /// Start an insert-mode WHOLE-LINE completion cycle (`i_CTRL-X CTRL-L`) and take the first step. Consumes
    /// the `CTRL-X` submode ([`insert_ctrl_x_pending`](Self::insert_ctrl_x_pending)) and reuses the same
    /// [`Completion`] cycle machinery as [`complete_start`](Self::complete_start) — the ONLY difference is
    /// the SOURCE: `base` and `cands` are whole-line (post-indent) text resolved by the frontend via
    /// [`Workspace::line_completion`](ruse_core::Workspace::line_completion). Always steps FORWARD onto the
    /// first matching line (Vim's `CTRL-X CTRL-L`); a bare `CTRL-L` (or `CTRL-N`/`CTRL-P`) then continues the
    /// cycle. With NO candidates this is a no-op ([`Feed::Ignored`]) and no cycle is armed (Vim bells).
    pub fn complete_line_start(&mut self, base: String, cands: Vec<String>) -> Feed {
        self.insert.ctrl_x = false; // the submode's job is done — the cycle owns continuation from here
        self.complete_start(base, cands, true)
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

    /// `{count}u` / `{count}<C-r>`: the command `count` times, so the step past the oldest (newest) change
    /// fails and stops a macro or `:normal`, as Vim's `u_doit` loop does (#1111). Capped: nvim keeps at
    /// most `'undolevels'` (1000) steps, so a hostile count cannot allocate without bound.
    fn counted(&mut self, cmd: Command) -> Feed {
        let n = self.mcount().min(10_000) as usize;
        self.reset();
        if n == 1 {
            Feed::Cmd(cmd)
        } else {
            Feed::Replay(vec![cmd; n])
        }
    }

    /// `gh`/`gH`/`g CTRL-H`: enter Visual of `kind` then switch it to Select, so the anchor is set as for
    /// `v`. A count selects that many chars (`3gh`) or lines (`2gH`), as nvim does with no prior Visual.
    fn enter_select(&mut self, kind: SelectKind) -> Feed {
        let extra = self.mcount() - 1;
        self.reset();
        let mut cmds = vec![Command::EnterVisual { kind }];
        if extra > 0 {
            let motion = if kind == SelectKind::Linewise {
                Motion::Down
            } else {
                Motion::Right
            };
            cmds.push(Command::Move(extra, motion));
        }
        cmds.push(Command::EnterSelect { kind });
        Feed::Replay(cmds)
    }

    /// Dispatch a PURE insert-entry (`i`/`a`/`I`/`A`/`o`/`O`, `R`/`gR`), capturing the leading count for
    /// count-on-insert repetition (VIM-CNT-INS) BEFORE [`action`](Self::action) resets the axes. The
    /// captured count is stashed in `pending_insert_count`; [`record`](Self::record) folds it into the
    /// [`ChangeIntent`] this entry opens, and the terminating `<Esc>` replays the typed text that many
    /// times. Change-family entries (`c`/`s`/…) do NOT come through here, so their count never repeats text.
    fn insert_entry(&mut self, cmd: Command) -> Feed {
        self.pending_insert_count = self.mcount();
        self.action(cmd)
    }

    /// A named-mark key after `` ` ``/`'`: with an operator pending it OPERATES to the mark
    /// (`` d`a ``/`d'a`, `` g~`a ``, `` >`a ``, `` =`a ``, `` gq`a ``); otherwise it JUMPS (`` `a ``/`'a`).
    /// `linewise` is the `'` form (honoured by delete/change/yank/case; the line operators are always
    /// linewise). The filter (`!'a`) hands the mark span to the frontend ([`Feed::FilterSpan`], #800).
    fn mark_op(&mut self, name: char, linewise: bool) -> Feed {
        let cmd = match self.normal.op.map(|p| p.op.class()) {
            None if linewise => Command::GotoNamedMarkLine(name),
            None => Command::GotoNamedMark(name),
            Some(class) => Command::OpToMark {
                op: class.mark_op().unwrap_or(MarkOp::Yank),
                name,
                linewise,
            },
        };
        self.span_action(cmd)
    }

    /// Emit `gn`/`gN` — the search-match text object. Reads the last search pattern and folds in any pending
    /// operator (`dgn`/`cgn`/`ygn`, `gUgn`, `>gn`, `gqgn`); the bare form ([`SearchOp::Move`]) selects the
    /// match in Visual. With no prior search the object carries an EMPTY pattern, which the core fails (a
    /// beep, so `.` keeps the earlier change) and the frontend reports as E35, as nvim does (#1184).
    /// Emitted as a single [`Command::SearchObject`] with the pattern baked in, so `.` can replay `cgn`.
    fn search_object(&mut self, backward: bool) -> Feed {
        let pattern = self
            .last_search
            .as_ref()
            .map(|(p, _, _)| p.clone())
            .unwrap_or_default();
        let count = self.mcount();
        self.span_action(Command::SearchObject {
            op: self.search_op(),
            count,
            pattern,
            backward,
        })
    }

    /// `*`/`#` (`whole_word`) and `g*`/`g#`: a search motion for the keyword under the cursor, carrying the
    /// count and any armed operator (`3*`, `d*`, `!#` — #1071). The planner resolves the keyword.
    fn word_search(&mut self, forward: bool, whole_word: bool) -> Feed {
        self.span_action(Command::SearchWordUnder {
            forward,
            whole_word,
            op: self.search_op(),
            count: self.mcount(),
        })
    }

    /// Arm `op` over the next motion, keeping the RAW count typed so far as the operator count.
    fn arm_op(&mut self, op: Op) -> Feed {
        self.normal.op = Some(OpPending {
            op,
            count: self.normal.count,
        });
        self.normal.count = 0;
        Feed::Pending
    }

    /// The doubled, LINEWISE form of the armed operator (`dd`, `3d2d`, `>>`, `==`, `guu`/`gugu`, `gqgq`,
    /// `!!`): it operates on the current `count` lines, where `count` multiplies EVERY count segment
    /// (`d3d` = 3, `2d2d` = 4 — #789). Called only with an operator armed.
    fn op_linewise(&mut self) -> Feed {
        let n = self.mcount();
        let Some(OpPending { op, .. }) = self.normal.op else {
            unreachable!("op_linewise called without an armed operator")
        };
        let line = Motion::Line;
        let feed = match op.class() {
            OpClass::Text(OpKind::Delete) => Feed::Cmd(Command::Delete(n, line)),
            OpClass::Text(OpKind::Change) => Feed::Cmd(Command::Change(n, line)),
            OpClass::Text(OpKind::Yank) => Feed::Cmd(Command::Yank(n, line)),
            OpClass::Case(case) => Feed::Cmd(Command::CaseMotion {
                count: n,
                motion: line,
                case,
            }),
            OpClass::Shift { left: false } => Feed::Cmd(Command::ShiftRight(n)),
            OpClass::Shift { left: true } => Feed::Cmd(Command::ShiftLeft(n)),
            OpClass::Reindent => Feed::Cmd(Command::Reindent {
                count: n,
                motion: line,
            }),
            OpClass::Format { keep } => Feed::Cmd(Command::Format {
                count: n,
                motion: line,
                keep_cursor: keep,
            }),
            OpClass::Comment => Feed::Cmd(Command::Comment {
                count: n,
                motion: line,
            }),
            OpClass::Filter => Feed::FilterMotion {
                count: n,
                motion: line,
            },
        };
        self.reset();
        feed
    }

    /// An operator key: arm it, or emit its linewise form when it repeats the armed one (`dd`/`cc`/`yy`).
    /// A DIFFERENT armed operator never reaches here — the operator-pending guard aborts it first (#790).
    fn operator(&mut self, op: Op) -> Feed {
        if self.normal.op.is_some_and(|p| p.op == op) {
            return self.op_linewise();
        }
        self.arm_op(op)
    }

    /// Feed one key given the current mode. Resolves the key into an outcome, then folds that outcome into
    /// the dot-repeat record (so `.` can later replay the last change). The two steps are split so the
    /// resolution grammar stays untouched by the recording concern.
    /// `:set (no)tildeop` — whether `~` is an operator ([`InputEngine::tildeop`]).
    pub fn set_tildeop(&mut self, on: bool) {
        self.tildeop = on;
    }

    /// Set `'history'`: the length of both the `:` and the search history (#1151).
    pub fn set_history_len(&mut self, len: usize) {
        self.ex_history.set_cap(len);
        self.search_history.set_cap(len);
    }

    /// `'history'`.
    #[must_use]
    pub fn history_len(&self) -> usize {
        self.ex_history.cap()
    }

    /// Whether `'tildeop'` is on.
    #[must_use]
    pub fn tildeop(&self) -> bool {
        self.tildeop
    }

    /// Install/replace one Lang-Arg mapping (`:lmap {lhs} {rhs}`, F-027). MVP is single-char → single-char.
    pub fn set_lang_mapping(&mut self, lhs: char, rhs: char) {
        self.lang_map.insert(lhs, rhs);
    }

    /// The Lang-Arg mapping of `lhs`, if any.
    #[must_use]
    pub fn lang_mapping(&self, lhs: char) -> Option<char> {
        self.lang_map.get(&lhs).copied()
    }

    /// Remove one Lang-Arg mapping (`:lunmap {lhs}`). A no-op if it was not mapped.
    pub fn clear_lang_mapping(&mut self, lhs: char) {
        self.lang_map.remove(&lhs);
    }

    /// Whether the language map is on for `mode` (plus the engine's live state): a command line whose own
    /// state is on, or, while the Insert state is on, the Insert namespace or a command reading a single
    /// character (`f`/`t`/`F`/`T`/`r`). Everything else —
    /// Normal, Visual, Operator-pending, Replace — is inert, which is the whole point of a SEPARATE Lang
    /// namespace (F-027 acceptance #2: "and to nothing else"): operators and motions are never translated.
    fn lang_eligible(&self, mode: Mode) -> bool {
        // Command-line namespace: the line's own state (a `/` line follows Insert's, a `:` line starts off;
        // `c_CTRL-^` flips it, #810), never inside a `c_CTRL-V` / `c_CTRL-K` collector.
        if let Some(cl) = self.cmdline.as_ref() {
            return cl.lang && cl.literal.is_none() && cl.digraph.is_none();
        }
        if !self.lang_active {
            return false;
        }
        // A command reading a single character as its argument: `f`/`t`/`F`/`T` (find target) or `r`
        // (replace). Vim's Lang-Arg translates that argument regardless of how the command was reached.
        if matches!(
            self.normal.awaiting,
            Awaiting::FindTarget { .. } | Awaiting::ReplaceChar | Awaiting::VirtualReplaceChar
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
        if self.lang_map.is_empty() {
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
            let out = self.feed_cmdline(key);
            // ...except the operator a search line completes: `d/pat<CR>` / `c/pat<CR>` is a change, and
            // `.` must replay IT, not an older one (#793). A bare `/pat<CR>` classifies as no change.
            if matches!(out, Feed::Cmd(Command::Search { .. })) && mode != Mode::Insert {
                self.record(&out, mode);
            }
            // ...and an expression's result: `<C-r>=` is typed text of the insert, `"=` selects the register
            // of the change that follows, so `.` repeats both (#808).
            if matches!(
                out,
                Feed::Cmd(Command::InsertEval(_) | Command::SetExprRegister(_))
            ) {
                self.record(&out, mode);
            }
            return out;
        }
        // An `i_CTRL-O` one-shot borrows the Normal grammar while the core mode still reads Insert; the
        // command it completes is a NORMAL command for dot-repeat, never part of the insert body (#795).
        let one_shot = mode == Mode::Insert && self.in_one_shot();
        // `i_CTRL-C` leaves Insert like `<Esc>` but is an interrupt, so it drops a count's repeats: nvim's
        // `3iab<C-c>` inserts `ab` once.
        let interrupt = mode == Mode::Insert
            && key.code == KeyCode::Char('c')
            && key.modifiers.contains(KeyModifiers::CONTROL);
        // `v_CTRL-O`: the Select one-shot lives only while the borrowed Visual command is being typed.
        let select_shot = std::mem::take(&mut self.select_shot);
        let out = self.feed_impl(key, mode);
        // A command that kept the selection (a motion, `o`, a shape switch) returns to Select. A change
        // ended it and runs as a plain command so `.` records it; `EnterSelect` is inert in the core once
        // the selection is gone (after `y`, `<Esc>`, `:`), so only "is this a change" needs deciding here.
        let select_return = match (select_shot, mode, &out) {
            (true, Mode::Visual { .. }, Feed::Pending) => {
                self.select_shot = true;
                None
            }
            (true, Mode::Visual { kind }, Feed::Cmd(cmd))
                if matches!(change_kind(cmd), ChangeKind::NotAChange) =>
            {
                Some(match cmd {
                    Command::EnterVisual { kind } => *kind,
                    _ => kind,
                })
            }
            _ => None,
        };
        if !one_shot {
            self.record(&out, mode);
        } else if !self.in_one_shot() {
            self.record(&out, Mode::Normal);
            // A one-shot that itself opened a change (`<C-o>cw`) owns the following text.
            if self.recording.is_some() {
                self.insert.resumed = false;
            }
        }
        // Count-on-insert (VIM-CNT-INS): a count-prefixed insert session that just closed on `<Esc>`
        // stages its extra repeats + trailing `EnterNormal` here — return them as one `Feed::Repeat` in
        // place of the bare `EnterNormal` so the whole `3ihello` collapses into a single undo group.
        // The tail REPLACES only the closing `EnterNormal`: a key that emitted more than that (`<C-v>65<Esc>`
        // resolves the code AND leaves Insert; `<C-@>` re-inserts AND leaves) keeps its leading commands (#797).
        if let Some(mut tail) = self.pending_insert_replay.take() {
            if !interrupt {
                let cmds = match out {
                    Feed::Cmd(Command::EnterNormal) => Vec::new(),
                    Feed::Replay(mut cmds) if cmds.last() == Some(&Command::EnterNormal) => {
                        cmds.pop();
                        cmds
                    }
                    other => {
                        debug_assert!(
                            false,
                            "an insert session closes only on EnterNormal, got {other:?}"
                        );
                        Vec::new()
                    }
                };
                tail.head = cmds;
                return Feed::Repeat(tail);
            }
        }
        match (select_return, out) {
            (Some(kind), Feed::Cmd(cmd)) => Feed::Replay(vec![cmd, Command::EnterSelect { kind }]),
            (_, out) => out,
        }
    }

    /// The Emacs profile's dispatch (F-012, minimal slice). Non-modal: the global-map's `C-` motions
    /// resolve to commands; an unmodified printable key inserts literally. A `C-u`/digit prefix argument
    /// (D-049) accumulates ahead of the command it modifies. This is the seam — the nine-tier stack, the
    /// `C-x` prefix maps, the kill ring and the mark ring layer on from here (RFC-0014). Motions work while
    /// the buffer stays editable because a `Move*` command keeps the current mode and only moves the cursor,
    /// so "move + insert in one state" needs no new mode.
    fn feed_emacs(&mut self, mut key: KeyEvent) -> Feed {
        // The `M-x` minibuffer owns the keystream while open (F-026 command-line namespace), reused verbatim
        // from the Vim `:`-line handler — its `<CR>` resolves the command NAME (the `cl.mx` branch).
        if self.cmdline.is_some() {
            return self.feed_cmdline(key);
        }
        // ESC is the meta prefix: the key after a bare ESC is that key with Meta held, however much later it
        // arrives (`ESC u` = `M-u`). Not while a `C-x` prefix is pending: that prefix owns its next key.
        if self.emacs_prefix.is_none() {
            if std::mem::take(&mut self.emacs_meta) {
                key.modifiers |= KeyModifiers::ALT;
            } else if key.code == KeyCode::Esc && key.modifiers.is_empty() {
                self.emacs_meta = true;
                return Feed::Pending;
            }
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        // A pending prefix key owns the NEXT keystroke: resolve it in that prefix's map before any global
        // dispatch. `C-g` (keyboard-quit) or any unbound key cancels the prefix (Emacs beeps). The prefix
        // is always cleared here, so a single stray key can never leave the engine wedged in a prefix.
        if let Some(prefix) = self.emacs_prefix.take() {
            return self.feed_emacs_prefix(prefix, key);
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
    /// [`emacs::EMACS_CX_MAP`] (`C-x C-s` saves, `C-x C-c` quits, `C-x u` undoes, …). An unbound key
    /// (including `C-g`) cancels the prefix and is inert — the prefix was already cleared by the caller, so
    /// the engine is never left wedged.
    fn feed_emacs_prefix(&mut self, prefix: char, key: KeyEvent) -> Feed {
        match prefix {
            'x' => emacs::emacs_cx_command(key).map_or(Feed::Ignored, Feed::Cmd),
            _ => Feed::Ignored,
        }
    }

    /// Fold a just-produced outcome into the dot-repeat record. In Insert mode, extend the in-flight change
    /// until `<Esc>` closes it; in Normal/Visual, an insert-entering command opens a recording, a
    /// self-contained edit becomes the record outright, and anything else leaves the record intact.
    /// `Pending`/`Ignored`/`Replay`/`Open*` never touch it — which is what makes `..` repeat one change.
    fn record(&mut self, out: &Feed, mode: Mode) {
        let Feed::Cmd(cmd) = out else {
            return;
        };
        self.replaced_change = None;
        // Replace / Virtual Replace (`R`/`gR`) are insert sessions too: their overwrites are the body.
        if matches!(mode, Mode::Insert | Mode::Replace | Mode::VirtualReplace) {
            // Insert resumed after an `i_CTRL-O` one-shot: the first edit opens a FRESH `i` change (Vim's
            // `stop_arrow` resets the redo buffer to `1i`); `<Esc>` with nothing typed keeps the old one.
            if std::mem::take(&mut self.insert.resumed)
                && self.recording.is_none()
                && *cmd != Command::EnterNormal
            {
                self.recording = Some(ChangeIntent {
                    lead: Command::EnterInsert,
                    insert: Vec::new(),
                    register: None,
                    expr: None,
                    entry_count: 1,
                });
            }
            if let Some(rec) = self.recording.as_mut() {
                rec.insert.push(cmd.clone());
                // The `<Esc>` that leaves Insert (recorded here so replay leaves Insert too) closes the change.
                if *cmd == Command::EnterNormal {
                    let closed = self.recording.take();
                    // Count-on-insert (VIM-CNT-INS): if a count preceded the entry, stage the extra repeats
                    // so `feed` returns them as ONE undo-grouped `Feed::Repeat` in place of this `<Esc>`.
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
            self.pending_record_expr = None;
            return;
        }
        // `"={expr}<CR>` is the register selection of the change that follows (#808).
        if let Command::SetExprRegister(expr) = cmd {
            self.pending_record_register = Some('=');
            self.pending_record_expr = Some(expr.clone());
            return;
        }
        // A search with no previous pattern (`dn`, `cn` before any search) fails before its operator runs, so
        // it is no change for `.` (nvim's `normal_search` → `clearopbeep`, #1173).
        let failing_search = matches!(cmd, Command::Search { pattern, .. } if pattern.is_empty());
        match if failing_search {
            ChangeKind::NotAChange
        } else {
            change_kind(cmd)
        } {
            ChangeKind::InsertEntering => {
                self.replaced_change = Some((self.last_change.clone(), self.recording.take()));
                self.recording = Some(ChangeIntent {
                    // `gi` repeats as a plain `i` at the NEW cursor (Vim's `nv_edit` turns `gi` into `i`
                    // after jumping to the `'^` mark), not as another jump to the last-insert spot.
                    lead: match cmd {
                        Command::InsertAtLastInsert => Command::EnterInsert,
                        other => other.clone(),
                    },
                    insert: Vec::new(),
                    register: self.pending_record_register.take(),
                    expr: self.pending_record_expr.take(),
                    // The count a pure insert-entry captured (`3i`); `0` (→ 1) for a count-less or
                    // change-family entry, so only `i`/`a`/`I`/`A`/`o`/`O` ever repeat their text.
                    entry_count: std::mem::take(&mut self.pending_insert_count).max(1),
                });
            }
            ChangeKind::Immediate => {
                self.replaced_change = Some((self.last_change.take(), self.recording.take()));
                self.last_change = Some(ChangeIntent {
                    lead: cmd.clone(),
                    insert: Vec::new(),
                    register: self.pending_record_register.take(),
                    expr: self.pending_record_expr.take(),
                    entry_count: 1,
                });
            }
            // A non-change (motion, mode switch, yank) forgets a dangling register selection.
            ChangeKind::NotAChange => {
                self.pending_record_register = None;
                self.pending_record_expr = None;
            }
        }
    }

    /// The command the engine just produced failed (the core beeped). When nvim cancels that command before
    /// it saves the redo buffer, the change it replaced comes back, so `.` still repeats the earlier change
    /// (#1179): a motion that fails under an operator (`d/zzz<CR>`, `dfz`, `d%`, `d'a` on an unset mark,
    /// `dk` on the first line) is cleared with `clearopbeep` before `do_pending_operator` runs `prep_redo`,
    /// and `J` on the last line, `r` past the end and `~` on an empty line bail out before theirs. A
    /// failure nvim reaches only after saving the redo (`CTRL-A` with no number, a put of an empty
    /// register, an operator refused by its register: `"/dd`) stays the change; `refused` says the core
    /// refused the operator for its register ([`ruse_core::Effect::RegisterRefused`]), which a motion
    /// failing first never reaches (`"/dfz` with no `z` keeps the earlier change, #1184). Call it only
    /// right after running the command `cmd` the engine returned, before the next key.
    pub fn change_failed(&mut self, cmd: &Command, refused: bool) {
        if refused || !repeat::fails_before_redo(cmd) {
            return;
        }
        let Some((last, recording)) = self.replaced_change.take() else {
            return;
        };
        self.last_change = last;
        self.recording = recording;
    }

    /// `i_CTRL-O` ENDS the in-flight insert change for dot-repeat, exactly as `<Esc>` would (Vim's
    /// `ins_ctrl_o` runs `ins_esc`): the typed text so far becomes `last_change` and the `".` text. An entry
    /// count's LIVE repetition is dropped (Vim zeroes it — `3iab<C-o>h` inserts `ab` once), though `.` of
    /// the ended change still repeats it (`3iab<C-o>0<Esc>j.` inserts `ababab`, nvim). The one-shot
    /// command then classifies as a Normal command, and the first edit after the return opens a fresh `i`
    /// change
    /// (`InsertState::resumed`, consumed in [`record`](Self::record)).
    /// Insert resumed by the frontend after a `:normal` run from `i_CTRL-O` (#1097): the keys ran in
    /// Normal mode, which dropped the Insert state, so mark the return as an `i_CTRL-O` one — the first
    /// edit opens a fresh change.
    pub fn resume_insert_after_one_shot(&mut self) {
        self.insert.resumed = true;
    }

    fn end_insert_change_for_one_shot(&mut self) {
        if let Some(mut rec) = self.recording.take() {
            rec.insert.push(Command::EnterNormal);
            self.last_inserted = rec.insert_body().to_vec();
            self.last_change = Some(rec);
        }
        self.insert.resumed = true;
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

    /// An Insert-mode cursor key (`i_<Left>`, `i_<Home>`, …). Vim treats it as the end of one insert and
    /// the start of another (verified vs nvim v0.12.6): the undo sequence breaks, so `u` undoes only what
    /// was typed after the move, and `.` repeats only that text as a plain `i`. The in-flight recording is
    /// therefore restarted here, which also drops a count-on-insert (`3ix<Left>y` inserts `y` once). The
    /// move itself is not recorded: it travels in the recorder-bypassing [`Feed::Replay`].
    fn insert_cursor_move(&mut self, cmd: Command) -> Feed {
        self.restart_insert_change();
        Feed::Replay(vec![Command::BreakUndo, cmd])
    }

    /// End the insert change at a caret jump and start a fresh plain-`i` one (see
    /// [`insert_cursor_move`](Self::insert_cursor_move)).
    fn restart_insert_change(&mut self) {
        self.reset();
        if self.recording.is_some() {
            self.recording = Some(ChangeIntent {
                lead: Command::EnterInsert,
                insert: Vec::new(),
                register: None,
                expr: None,
                entry_count: 1,
            });
        }
    }

    /// An Insert-mode page key (`i_<PageDown>`, `i_<S-Up>`, …), which the frontend scrolls because it needs
    /// the window: it is a caret jump like `i_<Down>`, so it breaks the undo sequence and restarts the `.`
    /// record (nvim v0.12.6). Returns the undo break for the caller to run before the scroll.
    pub fn insert_page_jump(&mut self) -> Command {
        // The jump ends a completion cycle, keeping the selected text (#805).
        self.insert.completion = None;
        self.restart_insert_change();
        Command::BreakUndo
    }

    fn feed_insert(&mut self, key: KeyEvent) -> Feed {
        // Any key reaching `feed_insert` is NOT a completion-cycle key (`CTRL-N`/`CTRL-P` are intercepted by
        // the frontend before `feed`), so it ACCEPTS the current candidate and ends the cycle (Vim). The
        // accepted text already sits in the buffer; dropping the state is all that is needed.
        self.insert.completion = None;
        // One-key Insert memories: both are consumed by whatever key comes next.
        let last_typed = self.insert.last_typed.take();
        let keep_undo = std::mem::take(&mut self.insert.keep_undo);
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        // `i_CTRL-X` submode: the in-scope second key `CTRL-L` (whole-line completion) is intercepted by the
        // frontend BEFORE `feed_insert` (it needs the buffer), so any key that REACHES here cancels the
        // submode. A CONTROL-modified key is a would-be submode selector we do not support (the deferred
        // `CTRL-N`/`CTRL-P` local-keyword, `CTRL-F` filename sources): SWALLOW it as a clean cancel rather
        // than inserting a stray char. A non-ctrl key falls through and is processed as normal Insert input
        // (so `CTRL-X` then `<Esc>` still leaves Insert, `CTRL-X` then a letter inserts it).
        if self.insert.ctrl_x {
            self.insert.ctrl_x = false;
            if ctrl {
                self.reset();
                return Feed::Ignored;
            }
        }
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
                        || c == '#'
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
        // `CTRL-G` prefix: consume the second key (nvim v0.12.6). `u` breaks the undo group; `U` keeps the
        // NEXT `<Left>`/`<Right>` inside it; `j`/`<Down>`/`CTRL-J` and `k`/`<Up>`/`CTRL-K` move a line down /
        // up to the column where inserting started (a caret jump, like `<Down>`). Anything else aborts the
        // prefix without inserting (Vim beeps). Checked before the layer so the printable path never sees the
        // prefixed key.
        if self.insert.ctrl_g {
            self.insert.ctrl_g = false;
            let only_ctrl = key.modifiers - KeyModifiers::SHIFT == KeyModifiers::CONTROL;
            let line = match key.code {
                KeyCode::Char('j') if !is_chord(key) || only_ctrl => Some(true),
                KeyCode::Char('k') if !is_chord(key) || only_ctrl => Some(false),
                KeyCode::Down if key.modifiers.is_empty() => Some(true),
                KeyCode::Up if key.modifiers.is_empty() => Some(false),
                _ => None,
            };
            if let Some(down) = line {
                return self.insert_cursor_move(Command::InsertStartColumnLine { down });
            }
            return match key.code {
                // `action` clears the transient axes (like every other completed key), so no partial
                // Normal state can survive an Insert key.
                KeyCode::Char('u') if !is_chord(key) => self.action(Command::BreakUndo),
                KeyCode::Char('U') if !is_chord(key) => {
                    self.reset();
                    self.insert.keep_undo = true;
                    Feed::Pending
                }
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
            // A chord is not a digraph char either: it aborts like a non-`Char` key.
            let (KeyCode::Char(c), false) = (key.code, is_chord(key)) else {
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
            self.end_insert_change_for_one_shot();
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
        // `i_CTRL-X` — arm the completion submode; the next key selects the source. Only `CTRL-L` (whole-line,
        // current buffer) is in scope this slice, resolved by the frontend (see the run loop). The Emacs
        // profile's `C-x` prefix is a SEPARATE non-modal path (`emacs_prefix`); this fires only in Vim/Native
        // Insert, which is the only profile set that reaches `feed_insert`.
        if ctrl && key.code == KeyCode::Char('x') {
            self.reset();
            self.insert.ctrl_x = true;
            return Feed::Pending;
        }
        // `i_CTRL-K` — arm the digraph prefix; the next TWO printable keys select the digraph.
        if ctrl && key.code == KeyCode::Char('k') {
            self.reset();
            self.insert.digraph = Some(DigraphPending::First);
            return Feed::Pending;
        }
        // `i_CTRL-V` — arm literal / numeric char entry; the next key(s) select the form and code
        // (Normal-mode `CTRL-V` is blockwise Visual — a separate path; this only fires in Insert). `i_CTRL-Q`
        // is the same key (`:help i_CTRL-Q`), for terminals that take `CTRL-V` for themselves.
        if ctrl && matches!(key.code, KeyCode::Char('v' | 'q')) {
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
        // `0 CTRL-D` / `^ CTRL-D` (the `0`/`^` typed just before): delete the typed char and ALL indent of the
        // line; after `^` the next `<CR>` restores it under 'autoindent' (#617). The two edits travel in a
        // `Replay`, which bypasses the recorder, so fold them in by hand.
        if ctrl && key.code == KeyCode::Char('d') {
            if matches!(last_typed, Some('0' | '^')) {
                self.reset();
                let delete = if last_typed == Some('^') {
                    Command::InsertDeleteIndentRestore
                } else {
                    Command::InsertDeleteIndent
                };
                let cmds = vec![Command::DeleteBack, delete];
                for cmd in &cmds {
                    self.record(&Feed::Cmd(cmd.clone()), Mode::Insert);
                }
                return Feed::Replay(cmds);
            }
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
        // CTRL aliases for keys the Insert layer binds (nvim v0.12.6): `i_CTRL-C` and `<C-[>` leave Insert
        // like `<Esc>` (`feed` drops a count's repeats for `CTRL-C`), `i_CTRL-H` is `<BS>`, `i_CTRL-J` and
        // `<C-m>` are `<CR>`, `i_CTRL-I` is `<Tab>`. Any other unbound chord inserts nothing (`unmatched`).
        let key = match key.code {
            KeyCode::Char('c' | '[') if ctrl => plain(KeyCode::Esc),
            KeyCode::Char('h') if ctrl => plain(KeyCode::Backspace),
            KeyCode::Char('j' | 'm') if ctrl => plain(KeyCode::Enter),
            KeyCode::Char('i') if ctrl => plain(KeyCode::Tab),
            _ => key,
        };
        // `<Tab>` inserts whitespace to the next tabstop (spaces under `expandtab`, else a hard tab). Without
        // this, Tab falls through to the Insert unmatched policy, which only emits for `Char` keys — so a raw
        // `KeyCode::Tab` would silently do nothing.
        if key.code == KeyCode::Tab {
            return self.action(Command::InsertTab);
        }
        // `CTRL-G U` then `<Left>`/`<Right>`: a step that stays in the undo group and the `.` record.
        if keep_undo
            && key.modifiers.is_empty()
            && matches!(key.code, KeyCode::Left | KeyCode::Right)
        {
            return self.action(Command::InsertCaretStep {
                right: key.code == KeyCode::Right,
            });
        }
        // `<S-Left>`/`<C-Left>` and `<S-Right>`/`<C-Right>` move a word back / forward (across lines);
        // `<C-Home>` goes to the first char of the file, `<C-End>` after the last one. Caret jumps, like the
        // plain arrows (nvim v0.12.6). `<S-Up>`/`<S-Down>` page-scroll in the frontend.
        if key.modifiers == KeyModifiers::SHIFT || key.modifiers == KeyModifiers::CONTROL {
            match key.code {
                KeyCode::Left => {
                    return self.insert_cursor_move(Command::Move(1, Motion::WordBack))
                }
                KeyCode::Right => {
                    return self.insert_cursor_move(Command::Move(1, Motion::WordFwd))
                }
                KeyCode::Home if ctrl => {
                    return self.insert_cursor_move(Command::Move(1, Motion::GotoByte))
                }
                KeyCode::End if ctrl => {
                    self.restart_insert_change();
                    return Feed::Replay(vec![
                        Command::BreakUndo,
                        Command::Move(1, Motion::LastLine),
                        Command::MoveLineEnd,
                    ]);
                }
                _ => {}
            }
        }
        // The unmodified cursor and editing keys (`:help ins-special-special`).
        if key.modifiers.is_empty() {
            if let Some(cmd) = insert_cursor_command(key.code) {
                return self.insert_cursor_move(cmd);
            }
            match key.code {
                // `i_<Del>`: delete the char under the caret; at the end of a line, join the next one. It is
                // part of the insert: one undo step with the typed text, and `.` replays it.
                KeyCode::Delete => return self.action(Command::DeleteForward(1)),
                // `i_<Insert>`: toggle to Replace (`<Insert>` in Replace toggles back).
                KeyCode::Insert => return self.action(Command::EnterReplace),
                _ => {}
            }
        }
        if let Resolved::Bound { value, .. } = self.profile.stack(Ns::Insert).resolve(&key.code) {
            let cmd = value.clone();
            self.reset();
            return Feed::Cmd(cmd);
        }
        let out = self.unmatched(Ns::Insert, key);
        if let (KeyCode::Char(c), Feed::Cmd(Command::InsertChar(_))) = (key.code, &out) {
            self.insert.last_typed = Some(c);
        }
        out
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
        let Some(c) = self.resolve_char(base, value) else {
            // Invalid code point (a lone surrogate via `u`/`U`, say): nothing to prepend — just the term.
            return self.feed_impl(key, Mode::Insert);
        };
        // Emit the resolved char AHEAD of whatever the terminator produces. `Replay` is the only multi-command
        // channel and it bypasses the recorder, so fold the char into the dot-repeat record by hand FIRST —
        // before the terminator runs, since a terminator like `<C-a>` records its own commands (#797).
        self.record(&Feed::Cmd(Command::InsertChar(c)), Mode::Insert);
        match self.feed_impl(key, Mode::Insert) {
            Feed::Cmd(tc) => {
                self.record(&Feed::Cmd(tc.clone()), Mode::Insert);
                Feed::Replay(vec![Command::InsertChar(c), tc])
            }
            // A terminator that is itself a multi-command replay (`<C-a>` / `<C-@>`), already recorded.
            Feed::Replay(tcmds) => {
                let mut cmds = vec![Command::InsertChar(c)];
                cmds.extend(tcmds);
                Feed::Replay(cmds)
            }
            // The terminator armed a prefix / was ignored: only the resolved char applies now. Any Insert
            // prefix it just armed survives (the reset clears only Normal state).
            _ => {
                self.reset();
                Feed::Replay(vec![Command::InsertChar(c)])
            }
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
        // The Insert-family CTRL aliases (see `feed_insert`): `CTRL-C`/`<C-[>` = `<Esc>`, `CTRL-H` = `<BS>`,
        // `CTRL-J`/`<C-m>` = `<CR>`. Any other chord is unbound and overwrites nothing (#599).
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let key = match key.code {
            KeyCode::Char('c' | '[') if ctrl => plain(KeyCode::Esc),
            KeyCode::Char('h') if ctrl => plain(KeyCode::Backspace),
            KeyCode::Char('j' | 'm') if ctrl => plain(KeyCode::Enter),
            _ => key,
        };
        // `<Insert>` in Replace toggles to Insert (`:help i_<Insert>`).
        if key.code == KeyCode::Insert && key.modifiers.is_empty() && mode == Mode::Replace {
            return self.action(Command::EnterInsert);
        }
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
            KeyCode::Char(_) if is_chord(key) => Feed::Ignored,
            KeyCode::Char(c) if mode == Mode::VirtualReplace => {
                Feed::Cmd(Command::VirtualReplaceType(c))
            }
            KeyCode::Char(c) => Feed::Cmd(Command::ReplaceType(c)),
            _ => Feed::Ignored,
        }
    }

    /// A CONTROL/ALT chord in the Normal family (Normal, Visual, Select, operator-pending, an `i_CTRL-O`
    /// one-shot), reached only with `awaiting == Nothing`. Only the chords below are bound; every other one
    /// takes its namespace's unmatched policy and never decodes as the bare letter (#598, #1059). Ground
    /// truth is nvim v0.12.6 (the table in `input/chord_tests.rs` pins it). `<C-c>` is handled
    /// in `feed_impl`; the chords the frontend owns in Normal (`<C-w>`, `<C-d>`/`<C-u>`, `<C-e>`/`<C-y>`,
    /// `<C-b>`/`<C-f>`/`<C-l>`/`<C-p>`, `<C-]>`) are taken before the engine sees them.
    fn feed_chord(&mut self, key: KeyEvent, mode: Mode) -> Feed {
        let selection = match mode {
            Mode::Visual { kind } | Mode::Select { kind } => Some(kind),
            _ => None,
        };
        let select = matches!(mode, Mode::Select { .. });
        let op = self.normal.op.is_some();
        let only_ctrl = key.modifiers - KeyModifiers::SHIFT == KeyModifiers::CONTROL;
        let c = match key.code {
            KeyCode::Char(c) if only_ctrl => c,
            _ => '\0', // an ALT/SUPER/… chord: nothing is bound
        };
        match c {
            // `CTRL-G` toggles Visual<->Select over the SAME selection; in Normal it is file info (name /
            // [Modified] / line count / cursor percent), resolved + status-surfaced by the frontend.
            'g' if !op => match mode {
                Mode::Visual { kind } => self.action(Command::EnterSelect { kind }),
                Mode::Select { kind } => self.action(Command::EnterVisual { kind }),
                Mode::Normal => self.action(Command::FileInfo),
                _ => self.unmatched(Ns::Normal, key),
            },
            // `<C-[>` is `<Esc>`, `<C-m>` is `<CR>` (the kitty protocol sends them as chords).
            '[' => self.feed_base(plain(KeyCode::Esc), mode),
            // `CTRL-\` (a legacy terminal sends it as `Ctrl-4`) arms `CTRL-\ CTRL-N` / `CTRL-\ CTRL-G`.
            '\\' | '4' => {
                self.normal.awaiting = Awaiting::CtrlBackslash;
                Feed::Pending
            }
            'm' => self.feed_base(plain(KeyCode::Enter), mode),
            // Motions (`:help CTRL-H`, `CTRL-J`, `CTRL-N`, `CTRL-P`): `<C-h>` = `h`, `<C-j>`/`<C-n>` = `j`,
            // `<C-p>` = `k`, with a count and under an operator (`d<C-j>` = `dj`). In Select, `CTRL-H` deletes
            // the selection like `<BS>` and `CTRL-J` (`<NL>`) replaces it like `<CR>`; `CTRL-N`/`CTRL-P` still
            // extend it (non-printable movement, `:help Select-mode`).
            'h' if select => self.action(Command::DeleteSelection),
            'j' if select => self.action(Command::ReplaceSelection('\n')),
            'h' => self.motion(Motion::Left),
            'j' | 'n' => self.motion(Motion::Down),
            'p' => self.motion(Motion::Up),
            // `v_CTRL-O` in Select: run ONE Visual command, then return to Select (see `feed`).
            'o' if select => {
                let kind = selection.expect("Select is a selection mode");
                let out = self.action(Command::EnterVisual { kind });
                self.select_shot = true;
                out
            }
            // `CTRL-V` (and its alias `CTRL-Q`): blockwise Visual; in a selection it switches the shape (the
            // same key again leaves it); under an operator it forces the motion blockwise (`d<C-v>j`).
            'v' | 'q' => match selection {
                Some(SelectKind::Blockwise) => self.action(Command::EnterNormal),
                Some(_) => self.action(Command::EnterVisual {
                    kind: SelectKind::Blockwise,
                }),
                None if op => {
                    self.normal.forced_wise = Some(ForcedWise::Blockwise);
                    Feed::Pending
                }
                None => self.action(Command::EnterVisual {
                    kind: SelectKind::Blockwise,
                }),
            },
            // `CTRL-A` / `CTRL-X` — add ±count to the number under-or-after the cursor, or to the first
            // number on every selected line.
            'a' | 'x' if !op => {
                let delta = i64::from(self.mcount());
                let delta = if c == 'a' { delta } else { -delta };
                if selection.is_some() {
                    self.action(Command::IncrementSelection {
                        delta,
                        sequential: false,
                    })
                } else {
                    self.action(Command::IncrementNumber(delta))
                }
            }
            // `CTRL-O` / `CTRL-I` walk the jumplist back / forward; `CTRL-R` redoes. Normal only.
            'o' if !op && selection.is_none() => self.action(Command::GotoOlderJump),
            'i' if !op && selection.is_none() => self.action(Command::GotoNewerJump),
            'r' if !op && selection.is_none() => self.counted(Command::Redo),
            _ => {
                let ns = if op {
                    Ns::OperatorPending
                } else if select {
                    Ns::Select
                } else if selection.is_some() {
                    Ns::Visual
                } else {
                    Ns::Normal
                };
                self.unmatched(ns, key)
            }
        }
    }

    /// The base Normal-family dispatch, reached only with `awaiting == Nothing` (the tier above
    /// returned for the pending cases). Shared char-search initiators, then the count/operator/motion
    /// grammar and the mode-specific keys.
    fn feed_base(&mut self, key: KeyEvent, mode: Mode) -> Feed {
        // Operator-pending (Vim's `checkclearop`): a key that cannot continue the armed operator ABORTS the
        // whole construct and is consumed — `ydw` must not delete, `yx` must not cut, `dJ` must not join
        // (#790). Checked first, so no Normal-only arm below can run with an operator armed.
        // A chord is exempt: `feed_chord` owns the chord table and its own operator-pending rule (`d<C-j>`
        // continues, `d<C-k>` aborts), and `<C-Left>`/`<S-Right>` are word motions.
        if let Some(p) = self.normal.op {
            // `{count}<Del>` edits the count being typed (`d23<Del>w` = `d2w`), so it continues too.
            let count_edit = key.code == KeyCode::Delete && self.normal.count > 0;
            // `dp`/`do` are `:diffput`/`:diffget`; ruse has no diff mode, so they fail as nvim does outside
            // one (#1087).
            if p.op == Op::Delete
                && key.modifiers.is_empty()
                && matches!(key.code, KeyCode::Char('p' | 'o'))
            {
                self.error = Some("E99: Current buffer is not in diff mode");
            }
            if !is_chord(key) && !count_edit && !continues_op(p.op, key) {
                return self.unmatched(Ns::OperatorPending, key);
            }
        }
        // A chord resolves in its own table and never reaches the bare-key arms below (#598, #1059).
        if is_chord(key) && matches!(key.code, KeyCode::Char(_)) {
            return self.feed_chord(key, mode);
        }
        // `<S-Left>`/`<C-Left>` and `<S-Right>`/`<C-Right>` move by word (`b`/`w`), not by char. `<C-Home>` is
        // `gg`; `<C-End>` goes to line `{count}` (default the last) on its LAST char (nvim v0.12.6). The other
        // modified special keys keep their plain meaning (`<S-Up>`/`<S-Down>` page-scroll in the frontend).
        if !key.modifiers.is_empty() {
            let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
            match key.code {
                KeyCode::Left => return self.motion(Motion::WordBack),
                KeyCode::Right => return self.motion(Motion::WordFwd),
                KeyCode::Home if ctrl => return self.motion(Motion::GotoLine),
                KeyCode::End if ctrl => {
                    let line = if self.normal.typed() > 0 {
                        Motion::GotoLine
                    } else {
                        Motion::LastLine
                    };
                    // Under an operator it is linewise like `G` (nvim's is charwise-inclusive to the last
                    // char; the two agree whenever the start is at or before the first non-blank).
                    if self.normal.op.is_some() {
                        return self.motion(line);
                    }
                    let n = self.mcount();
                    self.reset();
                    return Feed::Replay(vec![
                        Command::Move(n, line),
                        Command::Move(1, Motion::LineEnd),
                    ]);
                }
                _ => {}
            }
        }
        // Select (`:help Select-mode`): EVERY printable key — letters, digits, `"`, `:`, space, all the keys
        // Visual binds — replaces the selection and enters Insert, so it must win before the shared
        // initiators below (#792). `<CR>` replaces it with a line break, `<BS>`/`<Del>` delete it. The
        // non-printable keys (arrows, `<Home>`/`<End>`, `<Esc>`) fall through to Visual's handling, so
        // movement extends the selection.
        if let Mode::Select { .. } = mode {
            match key.code {
                KeyCode::Char(c) => return self.action(Command::ReplaceSelection(c)),
                KeyCode::Enter => return self.action(Command::ReplaceSelection('\n')),
                KeyCode::Backspace | KeyCode::Delete => {
                    return self.action(Command::DeleteSelection)
                }
                _ => {}
            }
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
                    // A repeat: after `t`/`T` the core skips the char the cursor stopped before (#802).
                    return self.motion(Motion::FindChar {
                        ch,
                        forward,
                        till,
                        repeat: true,
                    });
                }
            }
            KeyCode::Char(',') => {
                if let Some((ch, forward, till)) = self.last_find {
                    // `,` repeats in the opposite direction.
                    return self.motion(Motion::FindChar {
                        ch,
                        forward: !forward,
                        till,
                        repeat: true,
                    });
                }
            }
            // Line jumps: `g` arms `gg`; `G` jumps to `{count}` (or the last line when no count). In Visual
            // too (Select returned above), so `gu`/`gU`/`g~` recase the selection and `gg`/`ge`/`g_` extend it.
            KeyCode::Char('g') => {
                self.normal.awaiting = Awaiting::GSecond;
                return Feed::Pending;
            }
            // The count may have been typed BEFORE the operator (`2dG` = `d2G`, #791): read every segment.
            KeyCode::Char('G') => {
                return if self.normal.typed() > 0 {
                    self.motion(Motion::GotoLine)
                } else {
                    self.motion(Motion::LastLine)
                };
            }
            // Bare `%` matches the bracket; `{count}%` is Vim's percentage jump (go to count% of the file).
            KeyCode::Char('%') => {
                return if self.normal.typed() > 0 {
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
                self.normal.awaiting = Awaiting::MarkJump { keep_jumps: false };
                return Feed::Pending;
            }
            // `m` — arm a mark SET; the next key names the mark (`a`–`z`).
            KeyCode::Char('m') => {
                self.normal.awaiting = Awaiting::SetMarkChar;
                return Feed::Pending;
            }
            // `'` — arm a LINEWISE mark jump; the next key names the mark (`.` or `a`–`z`).
            KeyCode::Char('\'') => {
                self.normal.awaiting = Awaiting::MarkJumpLine { keep_jumps: false };
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
        if let Mode::Visual { kind } | Mode::Select { kind } = mode {
            // `v`/`V`/`CTRL-V` (`feed_chord`) switch the selection SHAPE: pressing the key of the current shape leaves the
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
                // Visual `r{char}` — replace every selected char with the next key (Vim `v_r`). Arm the
                // shared ReplaceChar expectation; its resolution sees the selection mode and emits the
                // selection form. Keeps the count axis (unused here) like Normal `r`.
                KeyCode::Char('r') => {
                    self.normal.awaiting = Awaiting::ReplaceChar;
                    return Feed::Pending;
                }
                // Visual `p`/`P` — replace the selection with the register. `p` swaps the deleted text into
                // the unnamed register; `P` preserves it (paste the same thing over successive selections).
                KeyCode::Char(c @ ('p' | 'P')) => {
                    let count = self.mcount();
                    return self.action(Command::PasteSelection {
                        swap: c == 'p',
                        count,
                    });
                }
                // `o` swaps the selection's ends (cursor <-> anchor); the SAME text stays selected but a
                // later motion extends the other end. In Normal `o` is OpenBelow — here it is the swap.
                KeyCode::Char('o') => return self.action(Command::SwapSelectionEnds),
                // `O`: in a block, the other corner on the same row; otherwise the same as `o`.
                KeyCode::Char('O') if is_block => {
                    return self.action(Command::VisualOp(VisualOp::OtherCorner))
                }
                KeyCode::Char('O') => return self.action(Command::SwapSelectionEnds),
                // `/` and `?` open the search line; the match it lands on extends the selection, as any
                // motion does (`v_/`, #1113). In Select a printable key replaces the selection instead.
                KeyCode::Char(c @ ('/' | '?')) if matches!(mode, Mode::Visual { .. }) => {
                    return self.enter_search(c == '?')
                }
                // `n` / `N` repeat the last search as a motion that extends the selection.
                KeyCode::Char('n' | 'N') if matches!(mode, Mode::Visual { .. }) => {}
                // `:` leaves Visual (setting `'<`/`'>`) and opens the command line on `'<,'>`; `!` opens it
                // on `'<,'>!`, the filter over the selected lines (`:help v_:`, `v_!`).
                KeyCode::Char(c @ (':' | '!')) => {
                    let out = self.action(Command::VisualOp(VisualOp::LeaveAtStart));
                    self.open_filter_cmdline(if c == ':' { "'<,'>" } else { "'<,'>!" });
                    return out;
                }
                // Line operators over the selection (`:help v_>`, `v_J`, `v_=`): `{count}>` shifts `count`
                // levels. The uppercase forms work on whole lines; in a block `D`/`C` run to each line's end
                // and `X`/`Y` are the block `d`/`y` (nvim v0.12.6).
                KeyCode::Char(c @ ('>' | '<')) => {
                    return self.action(Command::VisualOp(VisualOp::Shift {
                        left: c == '<',
                        count: self.mcount(),
                    }))
                }
                KeyCode::Char('=') => return self.action(Command::VisualOp(VisualOp::Reindent)),
                KeyCode::Char('J') => {
                    return self.action(Command::VisualOp(VisualOp::Join { spaces: true }))
                }
                KeyCode::Char(c @ ('D' | 'C')) if is_block => {
                    let op = if c == 'D' {
                        OpKind::Delete
                    } else {
                        OpKind::Change
                    };
                    return self.action(Command::VisualOp(VisualOp::BlockToEol(op)));
                }
                KeyCode::Char('X') if is_block => return self.action(Command::DeleteSelection),
                KeyCode::Char('Y') if is_block => return self.action(Command::YankSelection),
                KeyCode::Char(c @ ('D' | 'X' | 'Y' | 'C' | 'S' | 'R')) => {
                    let op = match c {
                        'D' | 'X' => OpKind::Delete,
                        'Y' => OpKind::Yank,
                        _ => OpKind::Change,
                    };
                    return self.action(Command::VisualOp(VisualOp::Lines(op)));
                }
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
                // `<Space>`/`<BS>` extend a Visual selection like `l`/`h`, wrapping across lines. In Select
                // they stay with the namespace policy (a typed space replaces the selection).
                KeyCode::Char(' ') | KeyCode::Backspace
                    if matches!(mode, Mode::Visual { .. }) && key.modifiers.is_empty() => {}
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
                self.normal.count = push_count_digit(self.normal.count, d as u32 - '0' as u32);
                Feed::Pending
            }
            KeyCode::Char('0') if self.normal.count > 0 => {
                self.normal.count = push_count_digit(self.normal.count, 0);
                Feed::Pending
            }
            KeyCode::Char('0') => self.motion(Motion::LineStart),
            code if motion_key(code).is_some() => {
                self.motion(motion_key(code).expect("guarded by is_some"))
            }
            // With an operator armed, `v`/`V` (and `CTRL-V`, in `feed_chord`) FORCE the next motion's wise
            // (Vim `o_v`/`o_V`/`o_CTRL-V`): `dvj`, `dVe`, `d<C-v>j`. They stay operator-pending (the motion
            // still follows); `motion` emits `OpForced`. Bare (no operator) they enter Visual/Visual-line.
            KeyCode::Char('v') if self.normal.op.is_some() => {
                self.normal.forced_wise = Some(ForcedWise::Charwise);
                Feed::Pending
            }
            KeyCode::Char('V') if self.normal.op.is_some() => {
                self.normal.forced_wise = Some(ForcedWise::Linewise);
                Feed::Pending
            }
            KeyCode::Char('v') => self.action(Command::EnterVisual {
                kind: SelectKind::Charwise,
            }),
            KeyCode::Char('V') => self.action(Command::EnterVisual {
                kind: SelectKind::Linewise,
            }),
            KeyCode::Char('d') => self.operator(Op::Delete),
            // `gcc` — the doubled comment operator: its `c` is the doubling key, not `c` (#618).
            KeyCode::Char('c')
                if matches!(
                    self.normal.op,
                    Some(OpPending {
                        op: Op::Comment,
                        ..
                    })
                ) =>
            {
                self.op_linewise()
            }
            KeyCode::Char('c') => self.operator(Op::Change),
            KeyCode::Char('y') => self.operator(Op::Yank),
            // `>`/`<` are operators: doubled (`>>`) shifts the current `count` lines, and over a motion
            // (`>j`, `>ap`) shifts the motion's lines (always linewise).
            KeyCode::Char('>') => self.operator(Op::ShiftRight),
            KeyCode::Char('<') => self.operator(Op::ShiftLeft),
            // `=` reindents (bracket-depth): doubled `==` is linewise, `=motion`/`=ap` over a motion.
            KeyCode::Char('=') => self.operator(Op::Reindent),
            // `!{motion}{cmd}<CR>` — the filter operator (linewise, like `=`/`gq`); doubled `!!` (and
            // `{count}!!`) filters the current `count` lines. The frontend numbers the lines and opens the
            // `:{range}!` cmdline.
            KeyCode::Char('!') => self.operator(Op::Filter),
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
            // `X` = `dh`: delete `count` chars before the cursor, never past the line's first char.
            KeyCode::Char('X') if self.normal.op.is_none() => {
                self.action(Command::Delete(self.mcount(), Motion::Left))
            }
            // `<Space>` / `<BS>` move like `l` / `h` but wrap across lines (nvim's default 'whichwrap' is
            // `b,s`); under an operator see [`Motion::WrapRight`] / [`Motion::WrapLeft`].
            KeyCode::Char(' ') if key.modifiers.is_empty() => self.motion(Motion::WrapRight),
            KeyCode::Backspace if key.modifiers.is_empty() => self.motion(Motion::WrapLeft),
            // `{count}<Del>` drops the count's last digit (`:help N<Del>`); a bare `<Del>` is `x`.
            KeyCode::Delete if self.normal.count > 0 => {
                self.normal.count /= 10;
                Feed::Pending
            }
            KeyCode::Delete if self.normal.op.is_none() => self.action(Command::DeleteUnder(1)),
            // `<Insert>` = `i`.
            KeyCode::Insert if self.normal.op.is_none() => self.insert_entry(Command::EnterInsert),
            // `Z` arms `ZZ` / `ZQ`.
            KeyCode::Char('Z') if self.normal.op.is_none() => {
                self.normal.awaiting = Awaiting::ZSecond;
                Feed::Pending
            }
            // Doubled case operators (`guu` / `gUU` / `g~~` / `g??`): the second key repeats the operator to
            // make it linewise. With an operator armed only its doubling key passes the operator-pending
            // guard, so `u`/`U`/`~` here ARE the doubling key; `?` is also the backward search (`d?pat`).
            KeyCode::Char('u' | 'U' | '~') if self.normal.op.is_some() => self.op_linewise(),
            KeyCode::Char('?')
                if matches!(self.normal.op, Some(OpPending { op: Op::Rot13, .. })) =>
            {
                self.op_linewise()
            }
            // `gqq` / `gww` — the doubled reflow operators format the current `count` lines (reachable now
            // that the session's macro layer leaves a `q` to an armed operator, #679). Only the doubling key
            // passes the operator-pending guard, so an armed operator here IS `gq` / `gw`.
            KeyCode::Char('q' | 'w') if self.normal.op.is_some() => self.op_linewise(),
            KeyCode::Char('u') => self.counted(Command::Undo),
            // `U` — undo the latest changes on the last changed line (a count does nothing).
            KeyCode::Char('U') => self.action(Command::LineUndo),
            KeyCode::Tab => self.action(Command::GotoNewerJump),
            KeyCode::Char('r') => {
                self.normal.awaiting = Awaiting::ReplaceChar;
                Feed::Pending
            }
            KeyCode::Char('R') => self.insert_entry(Command::EnterReplace),
            // With 'tildeop' `~` is the `g~` operator (`~w`; `~~` is its line form, above).
            KeyCode::Char('~') if self.tildeop => self.arm_op(Op::CaseToggle),
            KeyCode::Char('~') => self.action(Command::ToggleCase(self.mcount())),
            KeyCode::Char('J') => self.action(Command::JoinLines(self.mcount())),
            // `n` repeats the last search in the SAME direction it was issued; `N` in the OPPOSITE. So
            // after `?foo`, `n` continues BACKWARD and `N` goes forward; after `/foo` they stay
            // forward-relative. Direction comes from the stored last-search flag. Both REAPPLY the stored
            // offset (`:help search-offset`), so they route through [`Command::Search`] (which carries the
            // offset) rather than the plain `SearchNext`/`SearchPrev`; the offset-aware match rule advances
            // past the current match even when the offset landed the cursor before it (`s-1`).
            KeyCode::Char('n') => self.repeat_search(false),
            KeyCode::Char('N') => self.repeat_search(true),
            // `*`/`#` — search the whole keyword under the cursor forward / backward; operator-aware
            // (`d*`). The planner reads the keyword from the buffer (the engine has no buffer).
            KeyCode::Char('*') => self.word_search(true, true),
            KeyCode::Char('#') => self.word_search(false, true),
            // Dot-repeat: replay the last recorded change at the current cursor (D-047). A leading `N`
            // overrides the change's count (Vim `3.`). `.` itself never rewrites the record (beyond the
            // numbered-register step below), so `..` repeats the same change; with no prior change it is a clean no-op (the Normal namespace's
            // `closed/ignore` policy — Vim rings the bell).
            KeyCode::Char('.') => {
                if let Some(intent) = self.last_change.as_mut() {
                    // A change that named `"1`-`"8` repeats with the next numbered register, and the record
                    // keeps it, so `"1p...` walks the delete ring (nvim `do_redo`, #816); `"9` stays.
                    if let Some(c @ '1'..='8') = intent.register {
                        intent.register = char::from_digit(c.to_digit(10).unwrap_or(0) + 1, 10);
                    }
                    let intent = intent.clone();
                    let count = (self.normal.typed() > 0).then_some(self.normal.typed());
                    self.reset();
                    intent.replay(count).into_feed()
                } else {
                    self.unmatched(Ns::Normal, key)
                }
            }
            KeyCode::Char('/') => self.enter_search(false),
            // `?` — open the BACKWARD search line. Reachable here only when no ROT13 operator is pending
            // (the guarded `g??` arm above wins that case); an armed d/c/y operator is captured so `d?pat`
            // works, exactly like `d/pat`.
            KeyCode::Char('?') => self.enter_search(true),
            // `{count}:` opens the line on the count's lines (`Feed::CountCmdline`).
            KeyCode::Char(':') => {
                let count = self.normal.typed();
                self.reset();
                if count > 0 {
                    return Feed::CountCmdline { count };
                }
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
        // `CTRL-C` is Vim's interrupt (`:help CTRL-C`): it cancels whatever is pending (count, operator,
        // awaited key) and leaves Visual / Select; in a clean Normal it does nothing. It must never reach the
        // bare-key arms, where it decoded as the `c` operator, so `<C-c><C-c>` was `cc` (#897).
        if normal_family
            && !self.in_one_shot()
            && key.code == KeyCode::Char('c')
            && key.modifiers.contains(KeyModifiers::CONTROL)
        {
            let had_register = self.normal.register;
            self.reset();
            return if mode == Mode::Normal {
                self.drop_aborted_register(had_register, Feed::Ignored)
            } else {
                self.action(Command::EnterNormal)
            };
        }
        // A chord is never the char an awaited key names (`f<C-a>` is not `fa`, `"<C-a>` is not register
        // `a`): it aborts the pending construct, as in nvim. Only the `g` and `CTRL-\` prefixes bind chords.
        if is_chord(key)
            && matches!(key.code, KeyCode::Char(_))
            && !matches!(
                self.normal.awaiting,
                Awaiting::Nothing | Awaiting::GSecond | Awaiting::CtrlBackslash
            )
        {
            self.normal.awaiting = Awaiting::Nothing;
            return self.unmatched(Ns::OperatorPending, key);
        }
        // --- Top-priority tier: a one-shot key-expectation resolves before any base-key handling. ---
        match self.normal.awaiting {
            Awaiting::FindTarget { forward, till } => {
                self.normal.awaiting = Awaiting::Nothing;
                return match key.code {
                    KeyCode::Char(ch) => {
                        self.last_find = Some((ch, forward, till));
                        self.motion(Motion::FindChar {
                            ch,
                            forward,
                            till,
                            repeat: false,
                        })
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
                // `g<Down>`/`g<Up>`/`g<Home>`/`g<End>` are `gj`/`gk`/`g0`/`g$` (`:help g<Down>`).
                let key = match key.code {
                    KeyCode::Down if key.modifiers.is_empty() => plain(KeyCode::Char('j')),
                    KeyCode::Up if key.modifiers.is_empty() => plain(KeyCode::Char('k')),
                    KeyCode::Home if key.modifiers.is_empty() => plain(KeyCode::Char('0')),
                    KeyCode::End if key.modifiers.is_empty() => plain(KeyCode::Char('$')),
                    _ => key,
                };
                let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
                // `g CTRL-G` and Visual `g CTRL-A`/`g CTRL-X` are the only `g` chords; any other one aborts
                // instead of reaching the bare `g{char}` arms (`g<C-e>` is not `ge`).
                let g_chord = match key.code {
                    KeyCode::Char('g') => true,
                    KeyCode::Char('a' | 'x') => matches!(mode, Mode::Visual { .. }),
                    KeyCode::Char('h') => mode == Mode::Normal,
                    _ => false,
                };
                if is_chord(key) && matches!(key.code, KeyCode::Char(_)) && !(ctrl && g_chord) {
                    return self.unmatched(Ns::OperatorPending, key);
                }
                // Under an operator only the `g` MOTIONS continue it, and the doubled form of the `g`
                // operators (`gugu`, `gUgU`, `g~g~`, `g?g?`, `gqgq`, `gwgw`) is its linewise form; any other
                // `g` command (`dgJ`, `dgp`, `ygu`, ...) aborts instead of running bare (#790).
                if let Some(p) = self.normal.op {
                    let KeyCode::Char(c) = key.code else {
                        return self.unmatched(Ns::OperatorPending, key);
                    };
                    let doubled = matches!(
                        (p.op, c),
                        (Op::CaseLower, 'u')
                            | (Op::CaseUpper, 'U')
                            | (Op::CaseToggle, '~')
                            | (Op::Rot13, '?')
                            | (Op::Format, 'q')
                            | (Op::FormatKeep, 'w')
                    );
                    if !ctrl && doubled {
                        return self.op_linewise();
                    }
                    let g_motion = matches!(
                        c,
                        'g' | 'e'
                            | 'E'
                            | '_'
                            | 'j'
                            | 'k'
                            | '0'
                            | '$'
                            | '^'
                            | 'm'
                            | 'o'
                            | 'M'
                            | 'n'
                            | 'N'
                            | '*'
                            | '#'
                            | '`'
                            | '\''
                    );
                    if ctrl || !g_motion {
                        return self.unmatched(Ns::OperatorPending, key);
                    }
                }
                return match key.code {
                    // `g CTRL-G` — cursor position / buffer counts (Col/Line/Word/[Char]/Byte). Checked
                    // before the bare `g` (=`gg`) arm, since that arm ignores the ctrl modifier. Frontend-
                    // resolved + status-surfaced; no buffer mutation.
                    KeyCode::Char('g') if ctrl => self.action(Command::CursorInfo),
                    // `ga` — the numeric value of the character under the cursor (`:ascii`/`:as` synonym).
                    // `!ctrl` keeps it distinct from the Visual `g CTRL-A` sequence-increment arm below.
                    KeyCode::Char('a') if !ctrl => self.action(Command::AsciiInfo),
                    // `g8` — the UTF-8 bytes of the character under the cursor.
                    KeyCode::Char('8') if !ctrl => self.action(Command::Utf8Info),
                    // `` g`{mark} `` / `g'{mark}` — a mark jump that leaves the jumplist alone (#1175); under an
                    // operator the same motion as `` `{mark} ``/`'{mark}`. The count and operator stay armed.
                    KeyCode::Char('`') if !ctrl => {
                        self.normal.awaiting = Awaiting::MarkJump { keep_jumps: true };
                        Feed::Pending
                    }
                    KeyCode::Char('\'') if !ctrl => {
                        self.normal.awaiting = Awaiting::MarkJumpLine { keep_jumps: true };
                        Feed::Pending
                    }
                    KeyCode::Char('g') => self.motion(Motion::GotoLine),
                    // `gh` / `gH` / `g CTRL-H` — start Select charwise / linewise / blockwise (`:help gh`).
                    KeyCode::Char(c @ ('h' | 'H'))
                        if mode == Mode::Normal && self.normal.op.is_none() =>
                    {
                        self.enter_select(if ctrl {
                            SelectKind::Blockwise
                        } else if c == 'H' {
                            SelectKind::Linewise
                        } else {
                            SelectKind::Charwise
                        })
                    }
                    // `gV` only matters after a mapping that reselects in Select mode; from Normal it does
                    // nothing (nvim v0.12.6).
                    KeyCode::Char('V') if mode == Mode::Normal && self.normal.op.is_none() => {
                        self.reset();
                        Feed::Ignored
                    }
                    // Visual `gJ` / `gq` — join the selected lines without spaces / reflow them.
                    KeyCode::Char('J') if matches!(mode, Mode::Visual { .. }) => {
                        self.action(Command::VisualOp(VisualOp::Join { spaces: false }))
                    }
                    KeyCode::Char('q') if matches!(mode, Mode::Visual { .. }) => {
                        self.action(Command::VisualOp(VisualOp::Format))
                    }
                    // Visual `gc` — comment / uncomment the selected lines (nvim's default `gc`, #618).
                    KeyCode::Char('c') if matches!(mode, Mode::Visual { .. }) => {
                        self.action(Command::VisualOp(VisualOp::Comment))
                    }
                    // `ge` / `gE` — backward to the end of the previous word / WORD (operator-aware via
                    // `motion`, so `dge` deletes back through the previous word-end).
                    KeyCode::Char('e') => self.motion(Motion::WordEndBack),
                    KeyCode::Char('E') => self.motion(Motion::BigWordEndBack),
                    // `g_` — to the last non-blank char of the line (`{count}g_` = count-1 lines down).
                    KeyCode::Char('_') => self.motion(Motion::LineLastNonBlank),
                    // Screen-line motions `gj`/`gk`/`g0`/`g^`/`gm`/`g$` (#615): they count in the window's
                    // screen rows ('wrap', 'showbreak', the gutter, `leftcol`), which the engine does not
                    // know, so the frontend resolves them (the operator, count and register stay armed) and
                    // finishes them with `screen_motion_to` / `screen_motion_lines` / `screen_motion_fail`.
                    KeyCode::Char('j') => self.screen_motion(ScreenMotion::Down),
                    KeyCode::Char('k') => self.screen_motion(ScreenMotion::Up),
                    KeyCode::Char('0') => self.screen_motion(ScreenMotion::Start),
                    KeyCode::Char('$') => self.screen_motion(ScreenMotion::End),
                    KeyCode::Char('^') => self.screen_motion(ScreenMotion::FirstNonBlank),
                    KeyCode::Char('m') => self.screen_motion(ScreenMotion::Middle),
                    // `[count]go` — go to the count-th byte of the buffer (operator-aware: `dgo`).
                    KeyCode::Char('o') => self.motion(Motion::GotoByte),
                    // `gM` — go to `count`% of the line by CHARACTER count (bare `gM` = the middle char,
                    // i.e. 50%). The count is a PERCENTAGE, not a repeat, so seed it into `count` (default 50)
                    // and let `motion` carry it through — exactly the count-as-percentage shape `{count}%`
                    // (`GotoPercent`) uses. Operator-aware (`dgM`); `op_over_motion` clears the count after.
                    KeyCode::Char('M') => {
                        if self.normal.typed() == 0 {
                            self.normal.count = 50; // bare `gM` = 50% of the line
                        }
                        self.motion(Motion::MidLine)
                    }
                    // `g*` / `g#` — like `*`/`#` but match the word ANYWHERE (no `\<…\>` boundaries).
                    KeyCode::Char('*') => self.word_search(true, false),
                    KeyCode::Char('#') => self.word_search(false, false),
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
                    KeyCode::Char('u') if self.normal.op.is_none() => self.arm_op(Op::CaseLower),
                    KeyCode::Char('U') if self.normal.op.is_none() => self.arm_op(Op::CaseUpper),
                    KeyCode::Char('~') if self.normal.op.is_none() => self.arm_op(Op::CaseToggle),
                    KeyCode::Char('?') if self.normal.op.is_none() => self.arm_op(Op::Rot13),
                    // `gq` / `gw` — arm the reflow operator over the next motion (`gqap`, `gwj`). Normal
                    // only. `gqgq` is the doubled line form (through the operator-pending `g` tier above).
                    KeyCode::Char('q') if matches!(mode, Mode::Normal) => self.arm_op(Op::Format),
                    // `gc` — arm the comment operator (`gcj`, `gcip`; `gcc` is its line form). The `gc`
                    // comment text object (`dgc`, `gcgc`) is not modelled: `gcgc` cancels (#618).
                    KeyCode::Char('c')
                        if matches!(mode, Mode::Normal) && self.normal.op.is_none() =>
                    {
                        self.arm_op(Op::Comment)
                    }
                    KeyCode::Char('w') if matches!(mode, Mode::Normal) => {
                        self.arm_op(Op::FormatKeep)
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
                    // `gI` — insert at column 1 (byte column 0), BEFORE all indentation (`I` stops at the
                    // first non-blank). Routed through `insert_entry` so `{count}gI` repeats the typed text.
                    KeyCode::Char('I') => self.insert_entry(Command::InsertColumnZero),
                    // `gr{char}` — CLASSIC-Vim virtual-replace of ONE (or `{count}`) char, then back to Normal
                    // (the one-shot of `gR`). NOTE: nvim 0.11+ maps `gr`/`grn`/`gra`/`grr` to LSP actions by
                    // default, but those are keymaps, not a built-in; ruse installs no such map and targets the
                    // classic-Vim built-in. Arm the char expectation; the count carries through untouched.
                    KeyCode::Char('r') => {
                        self.normal.awaiting = Awaiting::VirtualReplaceChar;
                        Feed::Pending
                    }
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
                    KeyCode::Char('R') => self.insert_entry(Command::EnterVirtualReplace),
                    // `g-` / `g+` — chronological undo-time travel across branches (F-005 #3).
                    KeyCode::Char('-') => self.action(Command::UndoOlder),
                    KeyCode::Char('+') => self.action(Command::UndoNewer),
                    // `gQ` — enter Ex mode (F-026 #3). `Q` alone is NOT Ex at the pinned Neovim
                    // revision, so only this two-key form opens it; the line re-prompts until `:visual`.
                    // Like `:`, it ends the command: the count is dropped and a `CTRL-O` one-shot returns
                    // to Insert once the Ex line closes. Under an operator or in Visual it only cancels
                    // (Vim's `checkclearopq`): `dgQ` edits nothing and opens no Ex mode (#806).
                    KeyCode::Char('Q') => {
                        let refused = self.normal.op.is_some()
                            || matches!(mode, Mode::Visual { .. } | Mode::Select { .. });
                        self.reset();
                        if refused {
                            return Feed::Ignored;
                        }
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
            Awaiting::VirtualReplaceChar => {
                self.normal.awaiting = Awaiting::Nothing;
                return match key.code {
                    // `{count}gr{char}` — one-shot virtual replace (tab-aware) of `count` chars, back to
                    // Normal. The count accumulated before `gr` is still live (the `gr` arm did not reset it).
                    // No `<CR>` form: classic `gr<CR>` is not the line-splitting `r<CR>` — the char is taken
                    // literally, so a bare Enter here simply aborts the pending construct.
                    KeyCode::Char(c) => self.action(Command::VirtualReplaceChar(self.mcount(), c)),
                    _ => self.unmatched(Ns::OperatorPending, key),
                };
            }
            // `ZZ` / `ZQ` are exactly `:x` / `:q!` (`:help ZZ`), so they run the same Ex quit policy: one
            // place decides window-close versus editor exit and the unsaved-changes refusals (E37/E162).
            // `CTRL-\ CTRL-N` / `CTRL-\ CTRL-G` go to Normal mode: they leave a selection and drop a count or
            // operator, and do nothing in Normal (nvim v0.12.6). Any other key aborts.
            Awaiting::CtrlBackslash => {
                self.normal.awaiting = Awaiting::Nothing;
                let only_ctrl = key.modifiers - KeyModifiers::SHIFT == KeyModifiers::CONTROL;
                return match key.code {
                    KeyCode::Char('n' | 'g') if only_ctrl => {
                        if mode == Mode::Normal {
                            self.reset();
                            Feed::Ignored
                        } else {
                            self.action(Command::EnterNormal)
                        }
                    }
                    _ => self.unmatched(Ns::OperatorPending, key),
                };
            }
            Awaiting::ZSecond => {
                self.normal.awaiting = Awaiting::Nothing;
                let line = match key.code {
                    KeyCode::Char('Z')
                        if key.modifiers - KeyModifiers::SHIFT == KeyModifiers::NONE =>
                    {
                        "x"
                    }
                    KeyCode::Char('Q')
                        if key.modifiers - KeyModifiers::SHIFT == KeyModifiers::NONE =>
                    {
                        "q!"
                    }
                    _ => return self.unmatched(Ns::OperatorPending, key),
                };
                self.reset();
                return Feed::ExecuteEx(line.to_string());
            }
            Awaiting::MarkJump { keep_jumps } => {
                self.normal.awaiting = Awaiting::Nothing;
                let jump = self.normal.op.is_none();
                // Only a named mark composes with an operator here (`` d`a ``); the special marks do not yet,
                // so under an operator they abort rather than jump with the operator dropped (#790).
                if self.normal.op.is_some()
                    && !matches!(
                        key.code,
                        KeyCode::Char('a'..='z' | 'A'..='Z' | '^' | '<' | '>')
                    )
                {
                    return self.unmatched(Ns::OperatorPending, key);
                }
                let feed = match key.code {
                    // `` `^ `` (where Insert stopped), and `` d`< ``/`` d`> `` under an operator (#1088).
                    KeyCode::Char('^') => self.mark_op('^', false),
                    KeyCode::Char(c @ ('<' | '>')) if self.normal.op.is_some() => {
                        self.mark_op(c, false)
                    }
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
                    KeyCode::Char(c @ ('a'..='z' | 'A'..='Z')) => self.mark_op(c, false),
                    // Any other mark name is not wired — abort the pending construct.
                    _ => self.unmatched(Ns::OperatorPending, key),
                };
                return keep_jumps_feed(feed, keep_jumps && jump);
            }
            Awaiting::SetMarkChar => {
                self.normal.awaiting = Awaiting::Nothing;
                return match key.code {
                    // `m{a-zA-Z}` — set a named mark at the cursor (an uppercase one stays with this
                    // buffer, #1088).
                    KeyCode::Char(c @ ('a'..='z' | 'A'..='Z')) => {
                        self.action(Command::SetNamedMark(c))
                    }
                    // The other marks `m` can set are deferred — abort.
                    _ => self.unmatched(Ns::OperatorPending, key),
                };
            }
            Awaiting::MarkJumpLine { keep_jumps } => {
                self.normal.awaiting = Awaiting::Nothing;
                let jump = self.normal.op.is_none();
                if self.normal.op.is_some()
                    && !matches!(
                        key.code,
                        KeyCode::Char('a'..='z' | 'A'..='Z' | '^' | '<' | '>')
                    )
                {
                    return self.unmatched(Ns::OperatorPending, key);
                }
                let feed = match key.code {
                    // `'^`, and `d'<`/`d'>` under an operator (#1088).
                    KeyCode::Char('^') => self.mark_op('^', true),
                    KeyCode::Char(c @ ('<' | '>')) if self.normal.op.is_some() => {
                        self.mark_op(c, true)
                    }
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
                    KeyCode::Char(c @ ('a'..='z' | 'A'..='Z')) => self.mark_op(c, true),
                    // Any other mark name is not wired — abort.
                    _ => self.unmatched(Ns::OperatorPending, key),
                };
                return keep_jumps_feed(feed, keep_jumps && jump);
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
                    // to hold as the pending register the next yank/delete/change/paste reads. A PREFIX, not
                    // a command: the count typed so far survives as its own segment (`3"ayy` yanks 3,
                    // `2"a3x` cuts 6) and an `i_CTRL-O` one-shot stays in flight for the command that follows
                    // (`i<C-o>"ap` pastes) — #796. `"0p`/`"1p`/`"-p` paste from read-only edit-history slots.
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
                            // frontend-synced value. A yank/delete naming one fails (read-only, #1022).
                            || c == '/'
                            || c == ':'
                            || c == '.'
                            || c == '%'
                            // `"#` — the alternate file name (`:help quote#`).
                            || c == '#' =>
                    {
                        self.normal.reg_count = mul_raw(self.normal.reg_count, self.normal.count);
                        self.normal.count = 0;
                        self.normal.register = true;
                        Feed::Cmd(Command::SetRegister(Some(c)))
                    }
                    // Any other name is unusable; a pending construct is in flight, so it is `closed/abort`
                    // (operator-pending), leaking no state.
                    _ => self.unmatched(Ns::OperatorPending, key),
                };
            }
            Awaiting::BracketPrefix { open_bracket } => {
                self.normal.awaiting = Awaiting::Nothing;
                // `]p`/`[i`/... are commands, not motions: under an operator they abort (`d]p`, #790).
                if self.normal.op.is_some()
                    && matches!(key.code, KeyCode::Char('p' | 'P' | 'i' | 'I'))
                {
                    return self.unmatched(Ns::OperatorPending, key);
                }
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
                    // Method (brace-block) motions — count/operator-aware via `self.motion`. `]m`/`[m` go to
                    // the next/previous method START (a `{`); `]M`/`[M` go to the next/previous method END (a
                    // `}`). The starting bracket (`]` vs `[`) fixes the direction; the case picks start vs end.
                    KeyCode::Char('m') if !open_bracket => self.motion(Motion::MethodStartFwd),
                    KeyCode::Char('m') if open_bracket => self.motion(Motion::MethodStartBack),
                    KeyCode::Char('M') if !open_bracket => self.motion(Motion::MethodEndFwd),
                    KeyCode::Char('M') if open_bracket => self.motion(Motion::MethodEndBack),
                    // Keyword-line lookup (`:help [i`) — DISPLAY the line(s) containing the keyword under
                    // the cursor (current buffer). `[i`/`]i` echo the count'th match (`[` from the top of
                    // the file, `]` below the cursor); `[I`/`]I` list all matches. The frontend resolves
                    // the keyword and scans the lines (the engine has no buffer), like `*`/`gd`.
                    KeyCode::Char('i') => self.action(Command::ShowKeywordLines {
                        above: open_bracket,
                        list: false,
                        count,
                    }),
                    KeyCode::Char('I') => self.action(Command::ShowKeywordLines {
                        above: open_bracket,
                        list: true,
                        count,
                    }),
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
use vim::continues_op;
pub(crate) use vim::{motion_key, text_object, Ns, VimProfile};

mod native;
pub(crate) use native::{native_leader_command, NATIVE_LEADER_MENU};

mod emacs;
pub use emacs::emacs_command_by_name;
use emacs::{emacs_repeat, fold_emacs_count, EmacsArg, EmacsBinding, EmacsKey, EmacsProfile, Step};

mod cmdline;
mod cmdwin;
pub mod complete;
pub(crate) mod display_opts;
mod history;
pub use cmdline::display_with_caret;
use cmdline::{CmdLine, ExprTarget};
use cmdwin::CmdWin;
use history::CmdHistory;

pub(crate) mod digraph; // `pub(crate)`: the `:digraphs` listing overlay (ui) reads `digraph::entries`.
use digraph::digraph;

mod repeat;
pub use repeat::Repeat;
use repeat::{change_kind, ChangeIntent, ChangeKind};

mod ex;
mod ex_cmd;
mod ex_extra;
mod nvim_options;
pub(crate) mod set_cmd;
pub(crate) use ex::reuse_last_search;
pub(crate) use ex_cmd::full_command_name;
pub use ex_cmd::{strip_modifiers, ExModifiers};
pub use ex_extra::{FileCmd, PrintFlags, StartKind, ZSpec};
// `GlobalPayload` is referenced by the non-test dispatch/run-loop (routing `:g/pat/normal` vs `d`/`s`) and
// by the integration scenarios that drive `:g` directly, so it is part of the input surface like `Ex`.
pub use ex::{
    expand_bang, expand_filename, parse_ex, split_bar, BufTarget, EditSpec, Ex, ExpandError,
    FileFormatOpt, FileNames, FileOpts, FoldMethod, GlobalPayload, ReadSource, WriteSpec,
    WriteTarget,
};
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
/// The command an unmodified Insert-mode cursor key moves by. None of them leaves the line sideways:
/// `<Left>` stops at the first char and `<Right>` after the last ('whichwrap' has no `[`/`]` by default);
/// `<End>` rests after the last char and keeps that column wanted for `<Up>`/`<Down>`.
fn insert_cursor_command(code: KeyCode) -> Option<Command> {
    Some(match code {
        KeyCode::Left => Command::Move(1, Motion::Left),
        KeyCode::Right => Command::Move(1, Motion::Right),
        KeyCode::Up => Command::Move(1, Motion::Up),
        KeyCode::Down => Command::Move(1, Motion::Down),
        KeyCode::Home => Command::MoveLineStart,
        KeyCode::End => Command::MoveLineEnd,
        _ => return None,
    })
}

/// Split a typed search line into `{pattern}` and `{offset}` at the delimiter that ends the pattern, found
/// as Vim's `skip_regexp` finds it: a `\/` is a literal, and so is a delimiter inside `[...]` (`/[/]`,
/// #803). `None` when there is no offset part.
fn split_search_offset(line: &str, delim: char) -> (String, Option<SearchOffset>) {
    let idx = ruse_core::pattern::skip_regexp(line, delim, true);
    if idx < line.len() {
        (
            line[..idx].to_string(),
            Some(SearchOffset::parse(&line[idx + delim.len_utf8()..])),
        )
    } else {
        (line.to_string(), None)
    }
}

#[cfg(test)]
mod unit_tests;

#[cfg(test)]
mod chord_tests;

#[cfg(test)]
mod keymap_check;
