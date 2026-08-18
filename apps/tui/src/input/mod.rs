//! The input engine: a small pending-state machine that folds keys into semantic [`Command`]s
//! (`d`, `2w`, `d3w`, `dd`, `cw`=`ce`), plus ex-command (`:…`) parsing. The trace records the resulting
//! commands, so re-keymapping never invalidates a corpus. Pure and unit-tested.

use std::collections::HashMap;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ruse_core::keymap::{Layer, LayerStack, Resolved, UnmatchedKey};
use ruse_core::{
    BlockInsertKind, Command, ForcedWise, GlobalCmd, Mode, Motion, OpKind, SearchOp, SelectKind,
    SubFlags, SubRange, WordCase,
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
}

/// How a completed command relates to the dot-repeat record.
enum ChangeKind {
    /// Enters Insert; the change is this command PLUS the text typed until `<Esc>`.
    InsertEntering,
    /// A complete buffer edit with no insert session (`dw`, `x`, `dd`, `>>`, `~`, `r`, `p`).
    Immediate,
    /// Not a change (pure motion, mode switch, yank, undo/redo, search) — `.` leaves the record intact.
    NotAChange,
}

/// Classify a completed command for dot-repeat. Per Vim, yank is NOT dot-repeatable; delete/change/put/
/// replace/shift/`~`/join and the insert-entries ARE.
fn change_kind(cmd: &Command) -> ChangeKind {
    use Command as C;
    match cmd {
        // Insert-entering: the change includes the text typed until `<Esc>`.
        C::EnterInsert
        | C::EnterInsertAfter
        | C::InsertLineStart
        | C::AppendLineEnd
        | C::OpenBelow
        | C::OpenAbove
        | C::Change(..)
        | C::ChangeSelection
        | C::ReplaceSelection(_)
        | C::OpForced {
            op: OpKind::Change, ..
        } => ChangeKind::InsertEntering,
        // Self-contained buffer edits — dot-repeatable as a single command.
        C::Delete(..)
        | C::DeleteUnder(_)
        | C::DeleteForward(_)
        | C::DeleteBack
        | C::ReplaceChar(..)
        | C::ToggleCase(_)
        | C::JoinLines
        | C::ShiftRight(_)
        | C::ShiftLeft(_)
        | C::Paste { .. }
        | C::EmacsYank { .. }
        | C::EmacsKillLine
        | C::EmacsKillWord { .. }
        | C::EmacsBackwardKillWord { .. }
        | C::EmacsKillWholeLine
        | C::EmacsTransposeChars
        | C::EmacsTransposeWords
        | C::EmacsCaseWord { .. }
        | C::EmacsCaseRegion { .. }
        | C::EmacsDeleteIndentation
        | C::EmacsHorizontalSpace { .. }
        | C::EmacsOpenLine
        | C::DeleteSelection
        | C::OpForced {
            op: OpKind::Delete, ..
        } => ChangeKind::Immediate,
        // Everything else (motions, mode switches, yank incl. forced yank, search, undo/redo) is not a change.
        _ => ChangeKind::NotAChange,
    }
}

/// Rewrite a command's count for `N.` (Vim replaces the change's count with `N`). Commands without a count
/// are returned unchanged.
pub(crate) fn with_count(cmd: &Command, n: u32) -> Command {
    use Command as C;
    match cmd {
        C::Move(_, m) => C::Move(n, *m),
        C::Delete(_, m) => C::Delete(n, *m),
        C::Change(_, m) => C::Change(n, *m),
        C::Yank(_, m) => C::Yank(n, *m),
        C::OpForced {
            op, motion, wise, ..
        } => C::OpForced {
            op: *op,
            count: n,
            motion: *motion,
            wise: *wise,
        },
        C::DeleteUnder(_) => C::DeleteUnder(n),
        C::DeleteForward(_) => C::DeleteForward(n),
        C::ReplaceChar(_, c) => C::ReplaceChar(n, *c),
        C::ToggleCase(_) => C::ToggleCase(n),
        C::ShiftRight(_) => C::ShiftRight(n),
        C::ShiftLeft(_) => C::ShiftLeft(n),
        C::Paste { after, .. } => C::Paste {
            after: *after,
            count: n,
        },
        other => other.clone(),
    }
}

/// The motion a movement key names, shared by Normal (bare move / operator) and Visual (extend selection).
/// `0` is deliberately excluded — it is a count digit unless the count is empty, so callers special-case it.
fn motion_key(code: KeyCode) -> Option<Motion> {
    Some(match code {
        KeyCode::Char('h') | KeyCode::Left => Motion::Left,
        KeyCode::Char('l') | KeyCode::Right => Motion::Right,
        KeyCode::Char('j') | KeyCode::Down => Motion::Down,
        KeyCode::Char('k') | KeyCode::Up => Motion::Up,
        KeyCode::Char('w') => Motion::WordFwd,
        KeyCode::Char('b') => Motion::WordBack,
        KeyCode::Char('e') => Motion::WordEnd,
        KeyCode::Char('W') => Motion::BigWordFwd,
        KeyCode::Char('B') => Motion::BigWordBack,
        KeyCode::Char('E') => Motion::BigWordEnd,
        KeyCode::Char('$') => Motion::LineEnd,
        KeyCode::Char('^') => Motion::LineFirstNonBlank,
        KeyCode::Char('}') => Motion::ParagraphFwd,
        KeyCode::Char('{') => Motion::ParagraphBack,
        _ => return None,
    })
}

/// The text object a char names after `i`/`a`, or `None` if the char is not a text-object selector. `inner`
/// picks `i…` (interior) vs `a…` (around). Aliases collapse per Vim: `b`≡`(`≡`)`, `B`≡`{`≡`}`, `]`≡`[`, etc.
///
/// DEFERRED — `it`/`at` (tag objects): matching an HTML/XML tag needs a syntax/tree-sitter facility, which
/// is a FRONTEND concern (see `highlight.rs`) and is NOT wired into the dependency-free editor core. There is
/// no honest way to compute a tag range here, so `t`/`T` return `None` and route to the operator-pending
/// abort policy (a clean no-op) rather than a faked match. Re-enable once the core exposes a syntax tree.
fn text_object(ch: char, inner: bool) -> Option<Motion> {
    let around = !inner;
    let pair = |open, close| Motion::Pair {
        open,
        close,
        around,
    };
    Some(match ch {
        'w' if inner => Motion::InnerWord,
        'w' => Motion::AWord,
        'W' if inner => Motion::InnerBigWord,
        'W' => Motion::ABigWord,
        'p' if inner => Motion::InnerParagraph,
        'p' => Motion::AParagraph,
        's' if inner => Motion::InnerSentence,
        's' => Motion::ASentence,
        '(' | ')' | 'b' => pair('(', ')'),
        '{' | '}' | 'B' => pair('{', '}'),
        '[' | ']' => pair('[', ']'),
        '<' | '>' => pair('<', '>'),
        '"' => Motion::Quote { ch: '"', around },
        '\'' => Motion::Quote { ch: '\'', around },
        '`' => Motion::Quote { ch: '`', around },
        _ => return None,
    })
}

/// The Vim keymap namespaces this engine currently implements, as LAYERS of the D-045 stack.
///
/// Not all eight yet — Cmdline/Select/Terminal/Lang are not reachable from this engine — but the four
/// that are reachable now name themselves, and each carries its own unmatched-key policy instead of
/// sharing one `Feed::Ignored` fallthrough. That sharing was the shipped defect: one `closed/ignore`
/// standing in for five `open` policies (contracts/vim-style.yaml).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Ns {
    Normal,
    Insert,
    Visual,
    /// Shares Visual's selection state (they toggle with `CTRL-G`) and differs only in its
    /// unmatched-key policy: `open/replace-selection` where Visual is `closed/ignore`. Two namespaces
    /// over identical state, distinguished by the one dimension a transition table could not record.
    Select,
    /// Distinct from [`Ns::Normal`] precisely because its policy is `closed/abort`, not
    /// `closed/ignore` — the distinction the engine could not express while operator-pending was a
    /// field on Normal state rather than a namespace of its own.
    OperatorPending,
    /// The command-line namespace (`:`/`/`/`?`) — `open/append`, its own line editor (F-026). Declared
    /// here so it is addressable as one of the eight (VS-OBL-4 / KL-OBL-1); the live line editing is
    /// [`InputEngine::feed_cmdline`], which implements the append policy.
    Cmdline,
    /// The terminal-job namespace — `open/forward` (every key except the `CTRL-\` escape prefix goes to
    /// the job). Addressable now; live terminal buffers are deferred (no terminal buffer kind yet), so
    /// the forward policy is exercised at the router level, not from a running job.
    Terminal,
    /// The language-argument namespace (`:lmap`) — `open/translate` (rewrite the key through the active
    /// language map). Addressable here as a DECLARED policy, but the translation itself is REALISED by
    /// the pre-dispatch stage (`translate_lang`, above the layer stack), NOT inside `resolve` — so
    /// resolution stays total (D-048 / RFC-0013 answering KL-Q-LANG-ARG). F-027 is realised on that stage.
    Lang,
    /// Replace / Virtual-Replace mode (`R`/`gR`) — `open/overwrite` (a printable key overwrites; `<BS>`
    /// restores). NOT one of the eight canonical map-mode namespaces (Vim maps it under the `:map!`
    /// insert family); it is a distinct engine namespace here because its unmatched-key POLICY differs
    /// from Insert's — the same open/policy framing as Insert vs Select. It carries the `overwrite`
    /// policy so that policy is exercised through the router rather than a hardcoded early return.
    Replace,
}

impl Ns {
    fn id(self) -> &'static str {
        match self {
            Ns::Normal => "vim.normal",
            Ns::Insert => "vim.insert",
            Ns::Visual => "vim.visual",
            Ns::Select => "vim.select",
            Ns::OperatorPending => "vim.operator_pending",
            Ns::Cmdline => "vim.command_line",
            Ns::Terminal => "vim.terminal",
            Ns::Lang => "vim.lang_arg",
            Ns::Replace => "vim.replace",
        }
    }
}

/// The Vim profile's layer set: depth 1 and SEALED, declared rather than assumed (KL-OBL-3).
///
/// Depth-1-sealed is what makes Vim's guarantees hold. If an unsealed layer were ever installed
/// beneath these, `closed/ignore` would start falling through to it and every VS-OBL would break
/// without one line of Vim-specific code changing — so the property is asserted in the tests below
/// rather than left as a comment.
struct VimProfile {
    normal: LayerStack<KeyCode, Command>,
    insert: LayerStack<KeyCode, Command>,
    visual: LayerStack<KeyCode, Command>,
    select: LayerStack<KeyCode, Command>,
    operator_pending: LayerStack<KeyCode, Command>,
    command_line: LayerStack<KeyCode, Command>,
    terminal: LayerStack<KeyCode, Command>,
    lang: LayerStack<KeyCode, Command>,
    replace: LayerStack<KeyCode, Command>,
}

fn one(ns: Ns, policy: UnmatchedKey, binds: &[(KeyCode, Command)]) -> LayerStack<KeyCode, Command> {
    let mut layer = Layer::new(ns.id(), 100, true, policy);
    for (k, c) in binds {
        layer = layer.bind(*k, c.clone());
    }
    let mut stack = LayerStack::new();
    stack
        .push(layer)
        .expect("a single-layer stack cannot collide with itself");
    stack
}

impl VimProfile {
    fn new() -> VimProfile {
        VimProfile {
            // Normal and Visual hold no bindings HERE on purpose: their keys are a grammar
            // (count x operator x motion), not a flat table, and that grammar still lives in `feed`.
            // What the layer contributes today is the declared policy — which is the half that was
            // missing, not the half that worked.
            normal: one(Ns::Normal, UnmatchedKey::Ignore, &[]),
            visual: one(Ns::Visual, UnmatchedKey::Ignore, &[]),
            // Select carries no bindings either — its matched keys are Visual's grammar, shared in
            // `feed`. What it contributes is the OPPOSITE unmatched-key policy: a printable key that
            // matches nothing deletes the selection and enters Insert (`open/replace-selection`).
            select: one(Ns::Select, UnmatchedKey::ReplaceSelection, &[]),
            operator_pending: one(Ns::OperatorPending, UnmatchedKey::Abort, &[]),
            // Insert IS a flat table, so it is a real layer with real bindings — and routing it
            // through the stack is what removes the `if mode == Mode::Insert` early return.
            insert: one(
                Ns::Insert,
                UnmatchedKey::Insert,
                &[
                    (KeyCode::Esc, Command::EnterNormal),
                    (KeyCode::Enter, Command::InsertNewline),
                    (KeyCode::Backspace, Command::DeleteBack),
                ],
            ),
            // Command-line: `open/append`. Its keys (printable append, BS, CR, Esc) live in
            // `feed_cmdline` (F-026, a frontend line editor with no core Command), so the layer holds
            // no bindings — what it contributes is the declared append policy, making the namespace
            // addressable in its own right.
            command_line: one(Ns::Cmdline, UnmatchedKey::Append, &[]),
            // Terminal: `open/forward`. Addressable; no terminal buffer kind exists yet, so the layer
            // declares the policy and carries no bindings (the `CTRL-\` escape prefix is the only key
            // it would bind once live terminal buffers land).
            terminal: one(Ns::Terminal, UnmatchedKey::Forward, &[]),
            // Lang-Arg: `open/translate`. Addressable, but re-dispatch is CONCEPT-LANG-ARG (F-027).
            lang: one(Ns::Lang, UnmatchedKey::Translate, &[]),
            // Replace (`R`/`gR`): `open/overwrite`. Bindings shared with Insert (Esc/BS/CR); the
            // printable overwrite is the unmatched policy, applied via the router (not a hardcoded arm).
            replace: one(
                Ns::Replace,
                UnmatchedKey::Overwrite,
                &[
                    (KeyCode::Esc, Command::EnterNormal),
                    (KeyCode::Backspace, Command::ReplaceBackspace),
                    (KeyCode::Enter, Command::InsertNewline),
                ],
            ),
        }
    }

    fn stack(&self, ns: Ns) -> &LayerStack<KeyCode, Command> {
        match ns {
            Ns::Normal => &self.normal,
            Ns::Insert => &self.insert,
            Ns::Visual => &self.visual,
            Ns::Select => &self.select,
            Ns::OperatorPending => &self.operator_pending,
            Ns::Cmdline => &self.command_line,
            Ns::Terminal => &self.terminal,
            Ns::Lang => &self.lang,
            Ns::Replace => &self.replace,
        }
    }

    /// Every namespace the profile declares, for the addressability / depth-1-sealed assertions
    /// (VS-OBL-4 / KL-OBL-3) and the palette's binding reverse-lookup (F-004 #2). The eight Vim
    /// map-mode namespaces.
    fn all() -> [Ns; 8] {
        [
            Ns::Normal,
            Ns::OperatorPending,
            Ns::Insert,
            Ns::Cmdline,
            Ns::Visual,
            Ns::Select,
            Ns::Terminal,
            Ns::Lang,
        ]
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
    /// After `"`: the next key is the register NAME (`a`–`z`, or `A`–`Z` to append). It arms a one-shot
    /// pending register that the FOLLOWING yank/delete/change/paste targets — emitted as a
    /// [`Command::SetRegister`] the core applies before that command. `"` itself does not reset the count.
    RegisterSelect,
    /// After `>` / `<`: a matching second key completes the doubled shift operator (`>>` / `<<`); the
    /// `{count}` accumulated before it becomes the LINE count. Modelled like `gg`/`r` (a one-shot second
    /// key) rather than a full `Op` on the operator axis because only the doubled linewise form is wired —
    /// operator×motion (`>j`) is a deliberate carve-out, so a non-matching key aborts (operator-pending).
    ShiftSecond { right: bool },
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
    /// Sticky: the last search pattern, for `n`/`N`.
    last_search: Option<String>,
    /// Sticky: the last completed change, replayed by `.` (dot-repeat). `None` until the first change, so
    /// a bare `.` before any edit is a clean no-op.
    last_change: Option<ChangeIntent>,
    /// An in-flight change being recorded: set when an insert-entering command fires, then extended with the
    /// insert-session commands until the terminating `<Esc>` (`EnterNormal`) closes it into `last_change`.
    recording: Option<ChangeIntent>,
    /// The register named by the most recent `"x`, held only until the NEXT recorded change picks it up (so
    /// `.` replays it). Cleared by any intervening non-register command — a stray `"x` then a motion forgets.
    pending_record_register: Option<char>,
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

/// An Emacs prefix argument mid-read (F-012 / D-049). `C-u` seeds it (default 4); a further `C-u`
/// multiplies by four; a digit turns it into an explicit decimal count. The engine hands the finished
/// value to the next command OPAQUELY — Vim would fold an equivalent count as a motion multiplier, Emacs
/// lets each command interpret it (motions multiply, `C-u C-SPC` would pop a mark, etc.).
#[derive(Clone, Copy)]
struct EmacsArg {
    /// The accumulated numeric value.
    value: i32,
    /// True once an explicit digit was typed: later digits append decimally rather than re-seeding, and a
    /// following `C-u` stops multiplying (the digits ARE the literal count the user asked for).
    has_digits: bool,
}

impl EmacsArg {
    /// A bare `C-u` — the universal argument's default value of four.
    fn ctrl_u() -> EmacsArg {
        EmacsArg {
            value: 4,
            has_digits: false,
        }
    }

    /// Another `C-u` while no digit has been typed multiplies the running value by four (`C-u C-u` = 16).
    fn times_four(self) -> EmacsArg {
        if self.has_digits {
            self
        } else {
            EmacsArg {
                value: self.value.saturating_mul(4),
                has_digits: false,
            }
        }
    }

    /// A digit `0`–`9`: the first replaces the `C-u` seed, later ones append decimally (`C-u 3 7` = 37).
    fn push_digit(self, d: u32) -> EmacsArg {
        let d = d as i32;
        if self.has_digits {
            EmacsArg {
                value: self.value.saturating_mul(10).saturating_add(d),
                has_digits: true,
            }
        } else {
            EmacsArg {
                value: d,
                has_digits: true,
            }
        }
    }

    /// The count a multiplicative command (a motion) should use — clamped non-negative (negative args are
    /// not read yet; a `count` consumer never wants a negative repeat).
    fn count(self) -> u32 {
        self.value.max(0) as u32
    }
}

/// A resolved Emacs key: the code plus whether Control / Meta (Alt) / Shift were held. Emacs bindings are
/// fundamentally `modifier+key` (`C-f` ≠ `f`), so the keymap is keyed on this rather than a bare
/// [`KeyCode`] as the Vim namespaces are. Shift is tracked ONLY for non-character keys (`C-S-<backspace>`):
/// for a printable key Shift is already folded into the char (`Shift-2` = `@`), so tracking it there would
/// make a shifted printable miss its unshifted binding.
#[derive(Clone, Copy, PartialEq, Eq)]
struct EmacsKey {
    code: KeyCode,
    ctrl: bool,
    alt: bool,
    shift: bool,
}

impl EmacsKey {
    fn of(key: KeyEvent) -> EmacsKey {
        EmacsKey {
            code: key.code,
            ctrl: key.modifiers.contains(KeyModifiers::CONTROL),
            alt: key.modifiers.contains(KeyModifiers::ALT),
            // Only meaningful for non-char keys; a printable already encodes Shift in its char.
            shift: key.modifiers.contains(KeyModifiers::SHIFT)
                && !matches!(key.code, KeyCode::Char(_)),
        }
    }

    /// The one struct-building constructor the named ones below delegate to.
    fn with_mods(code: KeyCode, ctrl: bool, alt: bool, shift: bool) -> EmacsKey {
        EmacsKey {
            code,
            ctrl,
            alt,
            shift,
        }
    }

    fn ctrl(c: char) -> EmacsKey {
        EmacsKey::with_mods(KeyCode::Char(c), true, false, false)
    }

    fn alt(c: char) -> EmacsKey {
        EmacsKey::with_mods(KeyCode::Char(c), false, true, false)
    }

    fn plain(code: KeyCode) -> EmacsKey {
        EmacsKey::with_mods(code, false, false, false)
    }

    /// A Meta-modified non-character key, e.g. `M-DEL` (Meta+Backspace).
    fn alt_code(code: KeyCode) -> EmacsKey {
        EmacsKey::with_mods(code, false, true, false)
    }

    /// A Control-Shift-modified non-character key, e.g. `C-S-<backspace>` (kill-whole-line).
    fn ctrl_shift_code(code: KeyCode) -> EmacsKey {
        EmacsKey::with_mods(code, true, false, true)
    }
}

/// How a bound Emacs key folds the pending prefix argument (D-049) into a concrete command. The count
/// policy is per-binding — `C-k` (kill-line) ignores it, `M-d` (kill-word) multiplies — so the layer
/// stores the policy alongside the command rather than a bare [`Command`] the way the Vim layers do.
#[derive(Clone)]
enum EmacsBinding {
    /// A directional motion: the grapheme-aware bare command at count 1 (so the no-argument path matches
    /// the seam), a counted `Move(count, motion)` above. (`C-f`/`C-b`/`C-n`/`C-p`.)
    Directional { bare: Command, motion: Motion },
    /// A command rebuilt with the count in a native field. (`C-d`, `C-y`, `M-f`/`M-b`/`M-d`.)
    Counted(CountedCmd),
    /// A count-less command repeated `count` times via `Replay`. (`C-j`, `RET`, `DEL`.)
    Repeat(Command),
    /// A fixed command; the count is ignored. (`C-a`/`C-e` line ends, `C-/` undo, `M-<`/`M->` buffer ends,
    /// `C-k` kill-line.)
    Fixed(Command),
    /// Enter a prefix map — the next key resolves in that sub-map. (`C-x`.)
    Prefix(char),
    /// Open the `M-x` minibuffer to read a command name. (`M-x`.)
    Minibuffer,
}

/// A command whose count lives in a native field, named so the binding table stays declarative.
#[derive(Clone, Copy)]
enum CountedCmd {
    DeleteForward, // C-d — Emacs delete-char: DeleteForward(count), no kill-ring write (D-026)
    Yank,          // C-y — Emacs yank: EmacsYank { count } (paste + set mark, D-051)
    Move(Motion),  // M-f / M-b — Move(count, motion)
    KillWord,      // M-d — Emacs kill-word: EmacsKillWord { count } (accumulating kill, D-051)
    BackwardKillWord, // M-DEL — Emacs backward-kill-word: EmacsBackwardKillWord { count } (prepend kill)
}

/// Fold the pending prefix count into a bound key's command. `Prefix` is handled by the caller (it mutates
/// engine state); it is inert here.
fn fold_emacs_count(binding: &EmacsBinding, count: u32) -> Feed {
    match binding {
        EmacsBinding::Directional { bare, motion } => Feed::Cmd(if count == 1 {
            bare.clone()
        } else {
            Command::Move(count, *motion)
        }),
        EmacsBinding::Counted(c) => Feed::Cmd(match c {
            CountedCmd::DeleteForward => Command::DeleteForward(count),
            CountedCmd::Yank => Command::EmacsYank { count },
            CountedCmd::Move(m) => Command::Move(count, *m),
            CountedCmd::KillWord => Command::EmacsKillWord { count },
            CountedCmd::BackwardKillWord => Command::EmacsBackwardKillWord { count },
        }),
        EmacsBinding::Repeat(cmd) => emacs_repeat(cmd.clone(), count),
        EmacsBinding::Fixed(cmd) => Feed::Cmd(cmd.clone()),
        EmacsBinding::Prefix(_) | EmacsBinding::Minibuffer => Feed::Pending,
    }
}

/// The outcome of resolving one Emacs key, computed while `self.emacs` is borrowed and then acted on once
/// the borrow is released — so entering a prefix map (which mutates engine state) does not fight the
/// keymap borrow. `SelfInsert` carries the char the `global-map` self-insert policy matched.
enum Step {
    Bound(EmacsBinding),
    SelfInsert(char),
    Ignore,
}

/// The Emacs input profile (F-012 / D-045): ONE unsealed nine-tier layer stack, walked highest-priority
/// first, falling through to `global-map` at the bottom. This is the same [`LayerStack`] the Vim profile
/// uses — the shared resolution model D-045 posits — but arranged as Emacs's buffer-selected tier order
/// rather than Vim's eight sealed depth-1 namespaces. The eight upper tiers hold no bindings yet (no
/// minor/major modes exist); they are present so resolution walks the real order and future tiers slot in
/// by rank. `global-map`'s `Insert` policy IS `self-insert-command` — an unbound printable key self-inserts.
struct EmacsProfile {
    map: LayerStack<EmacsKey, EmacsBinding>,
}

/// The eight tiers above `global-map`, in Emacs's consultation order (highest priority first), ranked so
/// the stack walks them before it. Ids match the parity census (`emacs.keymaptier.NN.*`).
const EMACS_UPPER_TIERS: [(&str, u16); 8] = [
    ("emacs.keymaptier.01.overriding-terminal-local-map", 900),
    ("emacs.keymaptier.02.overriding-local-map", 800),
    ("emacs.keymaptier.03.keymap", 700),
    ("emacs.keymaptier.04.emulation-mode-map-alists", 600),
    ("emacs.keymaptier.05.minor-mode-overriding-map-alist", 500),
    ("emacs.keymaptier.06.minor-mode-map-alist", 400),
    ("emacs.keymaptier.07.local-map", 300),
    ("emacs.keymaptier.08.current-local-map", 200),
];

impl EmacsProfile {
    fn new() -> EmacsProfile {
        let mut map = LayerStack::new();
        for (id, rank) in EMACS_UPPER_TIERS {
            // Unsealed → transparent: an empty upper tier never stops the walk, so a miss always reaches
            // global-map, whose policy governs. `Ignore` is a placeholder these tiers never report today.
            map.push(Layer::new(id, rank, false, UnmatchedKey::Ignore))
                .expect("emacs tier ids and ranks are unique");
        }
        // global-map (tier 9): the base bindings + the self-insert policy. Unsealed like the rest so the
        // model stays uniform; being last, its policy is the one a miss reports.
        let global = Layer::new(
            "emacs.keymaptier.09.global-map",
            100,
            false,
            UnmatchedKey::Insert,
        )
        // Directional motions (count 1 keeps the grapheme-aware bare command = the #139 seam).
        .bind(
            EmacsKey::ctrl('f'),
            EmacsBinding::Directional {
                bare: Command::MoveRight,
                motion: Motion::Right,
            },
        )
        .bind(
            EmacsKey::ctrl('b'),
            EmacsBinding::Directional {
                bare: Command::MoveLeft,
                motion: Motion::Left,
            },
        )
        .bind(
            EmacsKey::ctrl('n'),
            EmacsBinding::Directional {
                bare: Command::MoveDown,
                motion: Motion::Down,
            },
        )
        .bind(
            EmacsKey::ctrl('p'),
            EmacsBinding::Directional {
                bare: Command::MoveUp,
                motion: Motion::Up,
            },
        )
        // Line ends / undo / buffer ends — count-agnostic.
        .bind(
            EmacsKey::ctrl('a'),
            EmacsBinding::Fixed(Command::MoveLineStart),
        )
        .bind(
            EmacsKey::ctrl('e'),
            EmacsBinding::Fixed(Command::MoveLineEnd),
        )
        .bind(EmacsKey::ctrl('/'), EmacsBinding::Fixed(Command::Undo))
        .bind(EmacsKey::ctrl('_'), EmacsBinding::Fixed(Command::Undo))
        .bind(
            EmacsKey::alt('<'),
            EmacsBinding::Fixed(Command::EmacsBufferEdge { start: true }),
        )
        .bind(
            EmacsKey::alt('>'),
            EmacsBinding::Fixed(Command::EmacsBufferEdge { start: false }),
        )
        // Kill-line: kill to end-of-line into the register, OR the terminating newline when already at EOL
        // (joining the next line) — its own `EmacsKillLine` command (D-051), not Vim `D`. The line-count
        // prefix argument is a follow-up, so it stays Fixed (count ignored).
        .bind(
            EmacsKey::ctrl('k'),
            EmacsBinding::Fixed(Command::EmacsKillLine),
        )
        // Transpose-chars: swap the two characters around point and advance (D-051, `EmacsTransposeChars`).
        // Count-less for now (the prefix-argument drag is a follow-up), so Fixed.
        .bind(
            EmacsKey::ctrl('t'),
            EmacsBinding::Fixed(Command::EmacsTransposeChars),
        )
        // C-o (open-line): insert a newline after point, leaving point before it. Prefix-agnostic, so Fixed.
        .bind(
            EmacsKey::ctrl('o'),
            EmacsBinding::Fixed(Command::EmacsOpenLine),
        )
        // M-t (transpose-words): swap the word around point with the following word. Prefix-agnostic, Fixed.
        .bind(
            EmacsKey::alt('t'),
            EmacsBinding::Fixed(Command::EmacsTransposeWords),
        )
        // M-^ (delete-indentation): join this line to the previous, fixing up whitespace. Fixed.
        .bind(
            EmacsKey::alt('^'),
            EmacsBinding::Fixed(Command::EmacsDeleteIndentation),
        )
        // M-@ (mark-word): set the mark at the end of the next word without moving point. Fixed.
        .bind(
            EmacsKey::alt('@'),
            EmacsBinding::Fixed(Command::EmacsMarkWord),
        )
        // M-} / M-{ (forward/backward-paragraph): reuse the shared paragraph motions (= Vim `}`/`{`).
        .bind(
            EmacsKey::alt('}'),
            EmacsBinding::Fixed(Command::Move(1, Motion::ParagraphFwd)),
        )
        .bind(
            EmacsKey::alt('{'),
            EmacsBinding::Fixed(Command::Move(1, Motion::ParagraphBack)),
        )
        // Case-word family: recase the word ahead and advance (D-051, `EmacsCaseWord`). Count-less for now
        // (the prefix-argument word count is a follow-up), so Fixed.
        .bind(
            EmacsKey::alt('u'),
            EmacsBinding::Fixed(Command::EmacsCaseWord {
                case: WordCase::Upcase,
            }),
        )
        .bind(
            EmacsKey::alt('l'),
            EmacsBinding::Fixed(Command::EmacsCaseWord {
                case: WordCase::Downcase,
            }),
        )
        .bind(
            EmacsKey::alt('c'),
            EmacsBinding::Fixed(Command::EmacsCaseWord {
                case: WordCase::Capitalize,
            }),
        )
        // Emacs region (D-027): set-mark, kill-region, kill-ring-save. `C-x C-x` (exchange) lives in the
        // C-x prefix map. `C-SPC` is bound as Ctrl+Space; some terminals deliver it as NUL — a delivery
        // detail for the frontend, not this map.
        .bind(EmacsKey::ctrl(' '), EmacsBinding::Fixed(Command::SetMark))
        .bind(
            EmacsKey::ctrl('w'),
            EmacsBinding::Fixed(Command::KillRegion),
        )
        .bind(EmacsKey::alt('w'), EmacsBinding::Fixed(Command::CopyRegion))
        // Counted edits: delete-char, yank, word motions, kill-word.
        .bind(
            EmacsKey::ctrl('d'),
            EmacsBinding::Counted(CountedCmd::DeleteForward),
        )
        .bind(EmacsKey::ctrl('y'), EmacsBinding::Counted(CountedCmd::Yank))
        .bind(
            EmacsKey::alt('f'),
            EmacsBinding::Counted(CountedCmd::Move(Motion::EmacsWordFwd)),
        )
        .bind(
            EmacsKey::alt('b'),
            EmacsBinding::Counted(CountedCmd::Move(Motion::WordBack)),
        )
        .bind(
            EmacsKey::alt('d'),
            EmacsBinding::Counted(CountedCmd::KillWord),
        )
        // M-DEL (backward-kill-word): kill the previous word; on a kill run it prepends onto the entry.
        .bind(
            EmacsKey::alt_code(KeyCode::Backspace),
            EmacsBinding::Counted(CountedCmd::BackwardKillWord),
        )
        // C-S-<backspace> (kill-whole-line): kill the whole line incl. its newline. Prefix-agnostic, Fixed.
        .bind(
            EmacsKey::ctrl_shift_code(KeyCode::Backspace),
            EmacsBinding::Fixed(Command::EmacsKillWholeLine),
        )
        // M-m (back-to-indentation): move to the first non-blank of the line. Prefix-agnostic, so Fixed.
        .bind(
            EmacsKey::alt('m'),
            EmacsBinding::Fixed(Command::Move(1, Motion::LineFirstNonBlank)),
        )
        // M-SPC just-one-space / M-\ delete-horizontal-space: collapse surrounding spaces/tabs.
        .bind(
            EmacsKey::alt(' '),
            EmacsBinding::Fixed(Command::EmacsHorizontalSpace { keep_one: true }),
        )
        .bind(
            EmacsKey::alt('\\'),
            EmacsBinding::Fixed(Command::EmacsHorizontalSpace { keep_one: false }),
        )
        // M-x opens the minibuffer to read a command name (execute-extended-command).
        .bind(EmacsKey::alt('x'), EmacsBinding::Minibuffer)
        // Newline / backspace — repeatable via Replay under a count.
        .bind(
            EmacsKey::ctrl('j'),
            EmacsBinding::Repeat(Command::InsertNewline),
        )
        .bind(
            EmacsKey::plain(KeyCode::Enter),
            EmacsBinding::Repeat(Command::InsertNewline),
        )
        .bind(
            EmacsKey::plain(KeyCode::Backspace),
            EmacsBinding::Repeat(Command::DeleteBack),
        )
        // Prefix map.
        .bind(EmacsKey::ctrl('x'), EmacsBinding::Prefix('x'));
        map.push(global)
            .expect("emacs tier ids and ranks are unique");
        EmacsProfile { map }
    }
}

/// Fold an Emacs prefix count into a command that has no native count field by repeating it: `count <= 1`
/// is the bare command, more is an ordered `Replay` (`C-u 3 RET` → three newlines). The Emacs path never
/// records, so `Replay` here is a pure "apply each in turn", not dot-repeat.
fn emacs_repeat(cmd: Command, count: u32) -> Feed {
    if count <= 1 {
        Feed::Cmd(cmd)
    } else {
        Feed::Replay(vec![cmd; count as usize])
    }
}

/// Resolve an Emacs command name to a [`Command`], for `M-x` (F-012). A minimal static registry covering the
/// commands the profile already binds — the depth-1 slice of F-004's fuller registry (completion, docstrings,
/// dynamic discovery are deferred). An unknown name returns `None` (Emacs "[No match]").
///
/// `pub` because it is the shared vocabulary between the M-x path and the Emacs parity comparator
/// (apps/tui/tests/emacs_parity_compare.rs): a fixture speaks Emacs command names, the oracle runs them in
/// Emacs, and ruse runs the SAME names through this registry — so one fixture drives both editors.
#[must_use]
pub fn emacs_command_by_name(name: &str) -> Option<Command> {
    Some(match name.trim() {
        "forward-char" => Command::MoveRight,
        "backward-char" => Command::MoveLeft,
        "next-line" => Command::MoveDown,
        "previous-line" => Command::MoveUp,
        "move-beginning-of-line" => Command::MoveLineStart,
        "back-to-indentation" => Command::Move(1, Motion::LineFirstNonBlank),
        "forward-paragraph" => Command::Move(1, Motion::ParagraphFwd),
        "backward-paragraph" => Command::Move(1, Motion::ParagraphBack),
        "just-one-space" => Command::EmacsHorizontalSpace { keep_one: true },
        "delete-horizontal-space" => Command::EmacsHorizontalSpace { keep_one: false },
        "open-line" => Command::EmacsOpenLine,
        "mark-word" => Command::EmacsMarkWord,
        "kill-whole-line" => Command::EmacsKillWholeLine,
        "upcase-region" => Command::EmacsCaseRegion {
            case: WordCase::Upcase,
        },
        "downcase-region" => Command::EmacsCaseRegion {
            case: WordCase::Downcase,
        },
        "capitalize-region" => Command::EmacsCaseRegion {
            case: WordCase::Capitalize,
        },
        "delete-indentation" => Command::EmacsDeleteIndentation,
        "move-end-of-line" => Command::MoveLineEnd,
        "beginning-of-buffer" => Command::EmacsBufferEdge { start: true },
        "end-of-buffer" => Command::EmacsBufferEdge { start: false },
        "forward-word" => Command::Move(1, Motion::EmacsWordFwd),
        "backward-word" => Command::Move(1, Motion::WordBack),
        "delete-char" => Command::DeleteForward(1),
        "kill-line" => Command::EmacsKillLine,
        "transpose-chars" => Command::EmacsTransposeChars,
        "transpose-words" => Command::EmacsTransposeWords,
        "upcase-word" => Command::EmacsCaseWord {
            case: WordCase::Upcase,
        },
        "downcase-word" => Command::EmacsCaseWord {
            case: WordCase::Downcase,
        },
        "capitalize-word" => Command::EmacsCaseWord {
            case: WordCase::Capitalize,
        },
        "kill-word" => Command::EmacsKillWord { count: 1 },
        "backward-kill-word" => Command::EmacsBackwardKillWord { count: 1 },
        "yank" => Command::EmacsYank { count: 1 },
        "newline" => Command::InsertNewline,
        "undo" => Command::Undo,
        "save-buffer" => Command::Save,
        "save-buffers-kill-terminal" => Command::Quit,
        "set-mark-command" => Command::SetMark,
        "kill-region" => Command::KillRegion,
        "kill-ring-save" => Command::CopyRegion,
        "exchange-point-and-mark" => Command::ExchangePointMark,
        _ => return None,
    })
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

/// The Native profile's leader (which-key) map (F-013 NAT-2) — the SEED of `native-profile@1`'s recommended
/// keymap. `<leader>` (Space) from a clean Normal base opens it; the next key resolves HERE to a semantic
/// command (INV-CMD-SEMANTIC) or aborts. It binds only commands that ALREADY exist — the Files/Git/Debug
/// discovery groups from the design (`docs/parity/native-style.md`) land as those features do, not before.
/// Intentionally-different from Vim/Emacs: a new discovery grammar, not a blend (NAT-2, D-051 spirit).
const NATIVE_LEADER_MENU: &[(char, &str, Command)] = &[
    ('w', "write", Command::Save),
    ('q', "quit", Command::Quit),
    ('u', "undo", Command::Undo),
    ('r', "redo", Command::Redo),
];

/// Resolve a leader selection key to its bound command, or `None` if the key is unbound — a which-key abort
/// (Emacs `C-g` / any key not on the menu closes it). Only an unmodified (or Shift-only) char key can bind.
fn native_leader_command(key: KeyEvent) -> Option<Command> {
    let KeyCode::Char(c) = key.code else {
        return None;
    };
    if !(key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT) {
        return None;
    }
    NATIVE_LEADER_MENU
        .iter()
        .find(|(k, _, _)| *k == c)
        .map(|(_, _, cmd)| cmd.clone())
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
            pending_record_register: None,
            pending_search: None,
            insert: InsertState::default(),
            activations: Vec::new(),
            cmdline: None,
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
        });
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
        });
    }

    /// Route a key into the open command-line buffer (F-026). The namespace owns the buffer: a printable
    /// key appends (open/append policy), `<BS>` deletes back, `<Esc>` aborts, `<CR>` finalises — a
    /// search folds through [`Self::submit_search`], an ex line becomes [`Feed::ExecuteEx`]. In `gQ` Ex
    /// mode the line re-opens after `<CR>` until `:visual`/`:vi`/an empty line exits.
    fn feed_cmdline(&mut self, key: KeyEvent) -> Feed {
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
                Feed::Pending
            }
            KeyCode::Char(c) => {
                cl.buffer.push(c);
                cl.cursor = cl.buffer.chars().count();
                Feed::Pending
            }
            KeyCode::Enter => {
                let prefix = cl.prefix;
                let ex_mode = cl.ex_mode;
                let mx = cl.mx;
                let text = std::mem::take(&mut cl.buffer);
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
                    cl.cursor = 0;
                    return Feed::ExecuteEx(text);
                }
                self.cmdline = None;
                if prefix == '/' {
                    self.submit_search(text)
                } else {
                    Feed::ExecuteEx(text)
                }
            }
            _ => Feed::Pending,
        }
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

    /// Complete a `/pattern` line (called by the command-line namespace on `<CR>`). Folds the pattern
    /// into the operator/count captured when `/` was pressed and yields the finished [`Command::Search`]:
    /// bare (`/pat`, `2/pat`) moves, or `d/pat`/`c/pat`/`y/pat` deletes/changes/yanks `[cursor, match)`.
    /// Also records the pattern for `n`/`N`. An empty pattern aborts (Vim's `<CR>` on an empty line is a
    /// no-op / reuse-last, which v0 treats as inert) — the armed operator is dropped, nothing is emitted.
    pub fn submit_search(&mut self, pattern: String) -> Feed {
        self.cmdline = None; // completing a search closes the command-line namespace (F-026)
        let (op, count) = self.pending_search.take().unwrap_or((SearchOp::Move, 1));
        if pattern.is_empty() {
            return Feed::Ignored;
        }
        self.last_search = Some(pattern.clone());
        Feed::Cmd(Command::Search { op, count, pattern })
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

    fn mcount(&self) -> u32 {
        self.normal.count.max(1)
    }

    /// Emit `m` — an operator command if one is armed, else a bare move — then clear the transient state.
    fn motion(&mut self, m: Motion) -> Feed {
        let cmd = match self.normal.op {
            Some(OpPending { op, count }) => {
                let total = count.max(1) * self.mcount();
                // A forced wise (`dvj`/`dVe`) resolves into `OpForced`; the `cw`->`ce` rewrite is a plain-
                // change nicety that does not apply to the (rare) forced-change form.
                if let Some(wise) = self.normal.forced_wise {
                    let opk = match op {
                        Op::Delete => OpKind::Delete,
                        Op::Change => OpKind::Change,
                        Op::Yank => OpKind::Yank,
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
                        // Vim `cw`/`cW` behave like `ce`/`cE` (do not eat the trailing space).
                        Op::Change => Command::Change(
                            total,
                            match m {
                                Motion::WordFwd => Motion::WordEnd,
                                Motion::BigWordFwd => Motion::BigWordEnd,
                                other => other,
                            },
                        ),
                        Op::Yank => Command::Yank(total, m),
                    }
                }
            }
            None => Command::Move(self.mcount(), m),
        };
        self.reset();
        Feed::Cmd(cmd)
    }

    fn action(&mut self, cmd: Command) -> Feed {
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
        // text) and NOT while an `i_CTRL-O` one-shot has borrowed the Normal grammar (that key is a
        // Normal command, not inserted text).
        mode == Mode::Insert && !self.insert.ctrl_g && !self.in_one_shot()
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
        // The command-line namespace owns the keystream while open (F-026); its typing is not a
        // dot-repeatable change, so it bypasses the recorder.
        if self.cmdline.is_some() {
            return self.feed_cmdline(key);
        }
        let out = self.feed_impl(key, mode);
        self.record(&out, mode);
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
                    self.last_change = self.recording.take();
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
                });
            }
            ChangeKind::Immediate => {
                self.recording = None;
                self.last_change = Some(ChangeIntent {
                    lead: cmd.clone(),
                    insert: Vec::new(),
                    register: self.pending_record_register.take(),
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
    fn feed_insert(&mut self, key: KeyEvent) -> Feed {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
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
        if let Resolved::Bound { value, .. } = self.profile.stack(Ns::Insert).resolve(&key.code) {
            let cmd = value.clone();
            self.reset();
            return Feed::Cmd(cmd);
        }
        self.unmatched(Ns::Insert, key)
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
        // here, before the shared `g` initiator below, so it is never mistaken for the start of `gg` — and
        // fully consumed in every mode: outside a selection nothing is bound (Vim's file-info `CTRL-G` is
        // not implemented), which is inert, NOT the start of `gg`.
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('g') {
            return match mode {
                Mode::Visual { kind } => self.action(Command::EnterSelect { kind }),
                Mode::Select { kind } => self.action(Command::EnterVisual { kind }),
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
            KeyCode::Char('%') => return self.motion(Motion::MatchBracket),
            // `"x` — arm register selection. Deliberately does NOT reset the count/operator axes, so a
            // count typed after it still lands (`"a3yy`). The next key is the register name (see the
            // `RegisterSelect` tier above). Shared by Normal and Visual (Vim supports `"ayiw` and `"xy`).
            KeyCode::Char('"') => {
                self.normal.awaiting = Awaiting::RegisterSelect;
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
                KeyCode::Char('d') | KeyCode::Char('x') => {
                    return self.action(Command::DeleteSelection)
                }
                KeyCode::Char('y') => return self.action(Command::YankSelection),
                KeyCode::Char('c') | KeyCode::Char('s') => {
                    return self.action(Command::ChangeSelection)
                }
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
            // `>`/`<` arm the doubled linewise shift; the second matching key emits it (see `ShiftSecond`).
            KeyCode::Char('>') => {
                self.normal.awaiting = Awaiting::ShiftSecond { right: true };
                Feed::Pending
            }
            KeyCode::Char('<') => {
                self.normal.awaiting = Awaiting::ShiftSecond { right: false };
                Feed::Pending
            }
            KeyCode::Char('p') => self.action(Command::Paste {
                after: true,
                count: self.mcount(),
            }),
            KeyCode::Char('P') => self.action(Command::Paste {
                after: false,
                count: self.mcount(),
            }),
            // Line-operator synonyms: `D`=`d$`, `C`=`c$`, `Y`=`y$` (nvim 0.6+ charwise), `S`=`cc`.
            // Each is the existing operator applied to an implicit motion, routed through the same
            // plan/commit path (so register geometry, cursor clamping, and dot-replayability match).
            KeyCode::Char('D') => self.action(Command::Delete(self.mcount(), Motion::LineEnd)),
            KeyCode::Char('C') => self.action(Command::Change(self.mcount(), Motion::LineEnd)),
            KeyCode::Char('Y') => self.action(Command::Yank(self.mcount(), Motion::LineEnd)),
            KeyCode::Char('S') => self.action(Command::Change(self.mcount(), Motion::Line)),
            KeyCode::Char('i') if self.normal.op.is_some() => {
                self.normal.awaiting = Awaiting::TextObjectChar { inner: true };
                Feed::Pending
            }
            KeyCode::Char('a') if self.normal.op.is_some() => {
                self.normal.awaiting = Awaiting::TextObjectChar { inner: false };
                Feed::Pending
            }
            KeyCode::Char('i') => self.action(Command::EnterInsert),
            KeyCode::Char('a') => self.action(Command::EnterInsertAfter),
            KeyCode::Char('I') => self.action(Command::InsertLineStart),
            KeyCode::Char('A') => self.action(Command::AppendLineEnd),
            KeyCode::Char('o') => self.action(Command::OpenBelow),
            KeyCode::Char('O') => self.action(Command::OpenAbove),
            KeyCode::Char('x') => self.action(Command::DeleteUnder(self.mcount())),
            KeyCode::Char('u') => self.action(Command::Undo),
            KeyCode::Char('r') if ctrl => self.action(Command::Redo),
            KeyCode::Char('r') => {
                self.normal.awaiting = Awaiting::ReplaceChar;
                Feed::Pending
            }
            KeyCode::Char('R') => self.action(Command::EnterReplace),
            KeyCode::Char('~') => self.action(Command::ToggleCase(self.mcount())),
            KeyCode::Char('J') => self.action(Command::JoinLines),
            KeyCode::Char('n') => match self.last_search.clone() {
                Some(p) => self.action(Command::SearchNext(p)),
                None => self.unmatched(Ns::Normal, key),
            },
            KeyCode::Char('N') => match self.last_search.clone() {
                Some(p) => self.action(Command::SearchPrev(p)),
                None => self.unmatched(Ns::Normal, key),
            },
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
            KeyCode::Char('/') => {
                // `/` is a MOTION, so an armed operator/count must survive the minibuffer (`d/pat`,
                // `2/pat`). Capture them (folded like `motion()` does: op-count × pending count) BEFORE
                // `reset()` clears the axes, then hand off to the frontend, which collects the pattern and
                // calls `submit_search` to build the finished command.
                let op = match self.normal.op {
                    Some(OpPending { op, .. }) => match op {
                        Op::Delete => SearchOp::Delete,
                        Op::Change => SearchOp::Change,
                        Op::Yank => SearchOp::Yank,
                    },
                    None => SearchOp::Move,
                };
                let count = match self.normal.op {
                    Some(OpPending { count, .. }) => count.max(1) * self.mcount(),
                    None => self.mcount(),
                };
                self.pending_search = Some((op, count));
                self.reset();
                self.open_cmdline('/', false);
                Feed::Pending
            }
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
                    // Not a text object (includes the deferred `t`/`T` tag objects): a pending construct is
                    // in flight, so this is `closed/abort` — the operator-pending policy (VS-OBL-3).
                    _ => self.unmatched(Ns::OperatorPending, key),
                };
            }
            Awaiting::GSecond => {
                self.normal.awaiting = Awaiting::Nothing;
                return match key.code {
                    KeyCode::Char('g') => self.motion(Motion::GotoLine),
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
                    // The count accumulated before `r` is still live (the `r` arm did not reset it).
                    KeyCode::Char(c) => self.action(Command::ReplaceChar(self.mcount(), c)),
                    // A pending construct is in flight, so this is `closed/abort` — the policy
                    // that distinguishes operator-pending from Normal (VS-OBL-3).
                    _ => self.unmatched(Ns::OperatorPending, key),
                };
            }
            Awaiting::RegisterSelect => {
                self.normal.awaiting = Awaiting::Nothing;
                return match key.code {
                    // A register name (`a`–`z` / `A`–`Z`, or the yank register `0`): emit `SetRegister` for
                    // the core to hold as the pending register the next yank/delete/change/paste reads.
                    // `action` clears the transient axes — which is why the register PREFIX must precede a
                    // count (`"a3yy`, as in Vim). `"0p` pastes the last yank (`"0` is read-only from edits).
                    KeyCode::Char(c) if c.is_ascii_alphabetic() || c == '0' => {
                        self.action(Command::SetRegister(Some(c)))
                    }
                    // The numbered delete-ring (`"1`–`"9`) and other registers are not modelled yet; a
                    // pending construct is in flight, so an unusable name is `closed/abort` (operator-
                    // pending), leaking no state.
                    _ => self.unmatched(Ns::OperatorPending, key),
                };
            }
            Awaiting::ShiftSecond { right } => {
                self.normal.awaiting = Awaiting::Nothing;
                // The count accumulated before the first `>`/`<` is still live and becomes the line count.
                return match key.code {
                    KeyCode::Char('>') if right => self.action(Command::ShiftRight(self.mcount())),
                    KeyCode::Char('<') if !right => self.action(Command::ShiftLeft(self.mcount())),
                    // Anything else (including the mismatched bracket, e.g. `><`): operator-pending abort.
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

mod cmdline;
use cmdline::CmdLine;

mod repeat;
use repeat::ChangeIntent;

mod ex;
pub use ex::{parse_ex, BufTarget, Ex};
#[cfg(test)]
pub(crate) use ex::{parse_substitute, GlobalSpec, SubSpec};

#[cfg(test)]
mod unit_tests;
