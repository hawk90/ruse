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
    /// An Emacs file or buffer command the session runs (#619): it asks questions or reads a file name.
    EmacsAction(emacs::EmacsAction),
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
        // Emacs `C-x C-c` is the palette's Quit, which the session runs as `save-buffers-kill-terminal`.
        Feed::EmacsAction(EmacsAction::KillTerminal) => &Command::Quit,
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

    /// Whether the Emacs profile is between commands: no minibuffer, prefix key, argument or meta pending.
    pub(crate) fn emacs_idle(&self) -> bool {
        self.cmdline.is_none()
            && self.emacs_prefix.is_none()
            && self.emacs_arg.is_none()
            && !self.emacs_meta
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
pub use emacs::{emacs_action_by_name, emacs_command_by_name, EmacsAction};
use emacs::{emacs_repeat, fold_emacs_count, EmacsArg, EmacsBinding, EmacsKey, EmacsProfile, Step};

mod cmdline;
mod cmdwin;
pub mod complete;
pub(crate) mod display_opts;
mod feed_cmdline;
mod feed_emacs;
mod feed_insert;
mod feed_normal;
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
pub(crate) use ex::{comment_free, delete_print_flags, reuse_last_search};
pub(crate) use ex_cmd::{full_command_name, is_substitute_word};
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
