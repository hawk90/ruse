//! The input engine: a small pending-state machine that folds keys into semantic [`Command`]s
//! (`d`, `2w`, `d3w`, `dd`, `cw`=`ce`), plus ex-command (`:…`) parsing. The trace records the resulting
//! commands, so re-keymapping never invalidates a corpus. Pure and unit-tested.

use std::collections::HashMap;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ruse_core::keymap::{Resolved, UnmatchedKey};
use ruse_core::VisualOp;
use ruse_core::{
    BlockInsertKind, Command, ForcedWise, GlobalCmd, LineAddr, Mode, Motion, OpKind, SearchOffset,
    SearchOp, SelectKind, SubRange, WordCase,
};
// The unit tests glob-import this module (`use crate::input::*`); the mark operator moved to `operator`.
#[cfg(test)]
use ruse_core::MarkOp;

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

/// Multiply two raw count segments (`3d2w` = 6), where `0` means "no count typed" (not a factor of zero),
/// clamped to [`MAX_COUNT`].
fn mul_raw(a: u32, b: u32) -> u32 {
    match (a, b) {
        (0, x) | (x, 0) => x,
        (a, b) => a.saturating_mul(b).min(MAX_COUNT),
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
    /// A screen-line motion handed to the frontend ([`Feed::ScreenMotion`]) and not finished yet: what
    /// the finisher needs to record the change for `.`, or the change `.` is repeating over it (#1539).
    /// Set only between the handoff and its finisher; [`feed`](Self::feed) drops a stale one.
    screen_handoff: Option<screen_motion::ScreenHandoff>,
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
    /// The last expression typed for the `"=` register (nvim's `expr_line`): at a `"=` or `<C-r>=` prompt
    /// or in `:put ={expr}` (#1342). An empty expression reuses it, as nvim re-reads the register's
    /// expression. Session-scoped frontend text like the history rings; the core only sees results.
    expr_line: String,
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
            screen_handoff: None,
            pending_search: None,
            insert: InsertState::default(),
            activations: Vec::new(),
            select_shot: false,
            cmdline: None,
            cmdwin: None,
            ex_history: CmdHistory::new(history::DEFAULT_CAP),
            search_history: CmdHistory::new(history::DEFAULT_CAP),
            expr_line: String::new(),
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

    /// Whether the next key starts a new command with nothing of one typed before it: [`Pending::Base`]
    /// with no count and no `"x` register prefix either. A `:normal` that ends here has finished its last
    /// command, so it gets no closing `<Esc>` (#1307).
    #[must_use]
    pub fn at_clean_boundary(&self) -> bool {
        self.pending() == Pending::Base && self.normal.is_clean()
    }

    /// Drop a `"x` register prefix with nothing typed after it (a count before it is kept by nvim but
    /// spent with it here), and say whether there was one. nvim's `"x` is a command of its own
    /// (`nv_regname`), so a `:normal` that ends on it has finished its last command; the register it
    /// named goes with the `:normal`'s own operator state (#1307). The caller clears the core's register.
    pub fn drop_bare_register(&mut self) -> bool {
        if self.pending() != Pending::Base || !self.normal.register || self.normal.count != 0 {
            return false;
        }
        self.normal = NormalState::default();
        true
    }

    fn action(&mut self, cmd: Command) -> Feed {
        self.reset();
        Feed::Cmd(cmd)
    }

    /// `{count}u` / `{count}<C-r>`: the command `count` times, so the step past the oldest (newest) change
    /// fails and stops a macro or `:normal`, as Vim's `u_doit` loop does (#1111). The cap is an allocation
    /// guard for a hostile count, not the undo bound: the core keeps `'undolevels'` + 1 groups (1000 by
    /// default, RFC-0026 §1) and the step past the oldest kept state fails, so with the default no count
    /// reaches it; only an `'undolevels'` raised past 9999 can make a count over 10000 stop short.
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

    /// `:set (no)tildeop` — whether `~` is an operator ([`InputEngine::tildeop`]).
    pub fn set_tildeop(&mut self, on: bool) {
        self.tildeop = on;
    }

    /// Set `'history'`: the length of both the `:` and the search history (#1151).
    pub fn set_history_len(&mut self, len: usize) {
        self.ex_history.set_cap(len);
        self.search_history.set_cap(len);
    }

    /// The expression an empty `"=` expression stands for: `expr` itself when it is not empty, else the
    /// last one ([`InputEngine::expr_line`] field, nvim's `expr_line`). A non-empty `expr` becomes the
    /// last one (#1342).
    pub fn take_expr_line(&mut self, expr: String) -> String {
        if expr.is_empty() {
            self.expr_line.clone()
        } else {
            self.expr_line.clone_from(&expr);
            expr
        }
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

    /// Feed one key given the current mode. Resolves the key into an outcome, then folds that outcome into
    /// the dot-repeat record (so `.` can later replay the last change). The two steps are split so the
    /// resolution grammar stays untouched by the recording concern.
    pub fn feed(&mut self, key: KeyEvent, mode: Mode) -> Feed {
        // Lang-Arg translation stage (F-027 / D-048): rewrite the key through the active language map
        // before ANY dispatch, in the three Lang-Arg contexts only. One substitution, then literal —
        // the translated key flows through normal dispatch and never re-enters this stage.
        let key = self.translate_lang(key, mode);
        // A handoff the frontend never finished (a test feeding past one) must not finish a later one.
        self.screen_handoff = None;
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
        if self.cmdline.is_some() {
            return self.feed_cmdline_recorded(key, mode);
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
        // A screen-line motion is finished by the frontend; its finisher records the change (#1539).
        if let Feed::ScreenMotion { motion, count } = out {
            self.arm_screen_handoff(motion, count, if one_shot { None } else { Some(mode) });
        }
        // A command that kept the selection (a motion, `o`, a shape switch) returns to Select. A change
        // ended it and runs as a plain command so `.` records it; `EnterSelect` is inert in the core once
        // the selection is gone (after `y`, `<Esc>`, `:`), so only "is this a change" needs deciding here.
        let select_return = self.select_return(select_shot, mode, &out);
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

    /// The command-line namespace owns the keystream while open (F-026); its typing is not a
    /// dot-repeatable change, so it bypasses the recorder.
    fn feed_cmdline_recorded(&mut self, key: KeyEvent, mode: Mode) -> Feed {
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
        out
    }

    /// The `v_CTRL-O` Select return for `out` (`select_shot` is the one-shot [`feed`](Self::feed) took):
    /// a command that kept the selection (a motion, `o`, a shape switch) returns to Select of the kind it
    /// names, and a still-pending command keeps the one-shot armed.
    fn select_return(&mut self, select_shot: bool, mode: Mode, out: &Feed) -> Option<SelectKind> {
        match (select_shot, mode, out) {
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
        }
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

mod binding_labels;
mod lang;
mod operator;
mod paste;
mod record;
mod screen_motion;
mod state;
mod unmatched;
use operator::{Op, OpPending};
pub use paste::{opens_control_sequence, paste_text};
pub use screen_motion::ScreenMotion;
use state::{
    Awaiting, Completion, DigraphPending, InsertState, LiteralBase, LiteralEntry, NormalState,
    Suspended,
};

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
#[cfg(test)]
use feed_cmdline::split_search_offset;
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

#[cfg(test)]
mod unit_tests;

#[cfg(test)]
mod chord_tests;

#[cfg(test)]
mod keymap_check;
