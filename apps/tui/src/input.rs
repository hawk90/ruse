//! The input engine: a small pending-state machine that folds keys into semantic [`Command`]s
//! (`d`, `2w`, `d3w`, `dd`, `cw`=`ce`), plus ex-command (`:…`) parsing. The trace records the resulting
//! commands, so re-keymapping never invalidates a corpus. Pure and unit-tested.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ruse_core::keymap::{Layer, LayerStack, Resolved, UnmatchedKey};
use ruse_core::{BlockInsertKind, Command, ForcedWise, Mode, Motion, OpKind, SearchOp, SelectKind};

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

/// A recorded **change-intent** for Vim dot-repeat (D-025 / D-047): the buffer-modifying command that
/// began the change, plus — for changes that enter Insert — the exact commands typed until `<Esc>`.
///
/// This is the design's key move: `.` records the INTENT (a re-parameterizable command + text), not a
/// resolved byte range, so replaying it at a new cursor re-runs the motion there. `dw` recorded, then `.`
/// at the next word deletes THAT word; `ciwFOO<Esc>` recorded, then `.` re-does the change AND re-inserts
/// `FOO`. `.` never overwrites the record, so `..` repeats the same change.
#[derive(Clone, PartialEq, Eq)]
struct ChangeIntent {
    /// The command that began the change: an operator (`dw`, `d2w`), a single-key edit (`x`, `~`, `>>`),
    /// or an insert-entry (`i`/`A`/`o`/`ciw`). Its count is the one `N.` overrides.
    lead: Command,
    /// The insert-session commands captured after an insert-entering `lead`, terminated by the
    /// `EnterNormal` that `<Esc>` produced. Empty for self-contained changes (`dw`, `x`, `>>`).
    insert: Vec<Command>,
    /// The register the change targeted (`"a` before it), replayed so `.` reuses the SAME register (Vim).
    /// `None` for an unregistered change; replay then omits the leading `SetRegister`.
    register: Option<char>,
}

impl ChangeIntent {
    /// The ordered command list `.` replays. `count` — a leading `N` on the `.` — REPLACES the lead's
    /// count (Vim `3.` repeats with count 3); `None` keeps the recorded count. Insert text is replayed
    /// verbatim.
    fn replay(&self, count: Option<u32>) -> Vec<Command> {
        let lead = match count {
            Some(n) => with_count(&self.lead, n),
            None => self.lead.clone(),
        };
        let mut cmds =
            Vec::with_capacity(1 + self.insert.len() + usize::from(self.register.is_some()));
        // Re-select the register first, so the replayed change writes to the same slot (`"ax` then `.`).
        if self.register.is_some() {
            cmds.push(Command::SetRegister(self.register));
        }
        cmds.push(lead);
        cmds.extend(self.insert.iter().cloned());
        cmds
    }
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
        | C::DeleteBack
        | C::ReplaceChar(..)
        | C::ToggleCase(_)
        | C::JoinLines
        | C::ShiftRight(_)
        | C::ShiftLeft(_)
        | C::Paste { .. }
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
fn with_count(cmd: &Command, n: u32) -> Command {
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
}

impl Ns {
    fn id(self) -> &'static str {
        match self {
            Ns::Normal => "vim.normal",
            Ns::Insert => "vim.insert",
            Ns::Visual => "vim.visual",
            Ns::Select => "vim.select",
            Ns::OperatorPending => "vim.operator_pending",
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
        }
    }

    fn stack(&self, ns: Ns) -> &LayerStack<KeyCode, Command> {
        match ns {
            Ns::Normal => &self.normal,
            Ns::Insert => &self.insert,
            Ns::Visual => &self.visual,
            Ns::Select => &self.select,
            Ns::OperatorPending => &self.operator_pending,
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

/// The Normal/Visual input state, held as three **orthogonal axes** — `count`, the operator-pending `op`,
/// and the one-shot `awaiting` key-expectation — plus sticky repeat state. `feed` resolves them in a fixed
/// precedence (mode → awaiting tier → base keys), so the hierarchy is explicit, not encoded in field order.
pub struct InputEngine {
    /// The active profile's layers. Built once — resolution must not allocate per keystroke.
    profile: VimProfile,
    /// Count axis: the accumulating numeric prefix for the next motion/operator.
    count: u32,
    /// Operator axis: an armed operator awaiting its motion (`None` = none).
    op: Option<OpPending>,
    /// Key-expectation axis: what the next key must supply (the top-priority resolution tier).
    awaiting: Awaiting,
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
    /// A forced motion wise armed by `v`/`V` after an operator (Vim `o_v`/`o_V`): the NEXT motion resolves
    /// into a [`Command::OpForced`] instead of a plain operator command. `None` unless mid-`dv…`/`dV…`.
    forced_wise: Option<ForcedWise>,
    /// Insert-mode `CTRL-O`: run exactly ONE Normal-mode command, then return to Insert (Vim `i_CTRL-O`).
    /// While set, `feed` routes keys through the Normal grammar instead of the Insert layer; a completed
    /// command clears it (via `reset`), so subsequent keys are Insert again. The core needs no help — every
    /// Normal editing/motion command inherits `st.mode` (Insert) and the Normal-only cursor clamp is gated
    /// on Normal mode, so the command applies correctly and the buffer stays in Insert on its own.
    insert_one_shot: bool,
    /// Insert-mode `CTRL-G` prefix: the next key is expected to be `u` (undo-break, [`Command::BreakUndo`]).
    /// A one-key expectation local to Insert; any other second key aborts the prefix (Vim beeps).
    insert_ctrl_g: bool,
    /// The command-line namespace (F-026): while `Some`, keys are routed into its owned line buffer
    /// rather than the Normal grammar. `None` = not on the command line. This is the engine owning the
    /// line, not an ad-hoc text buffer on the UI (anti-pattern command-line P2).
    cmdline: Option<CmdLine>,
}

/// The command-line namespace's owned state (F-026 acceptance #2): a prefix, a line buffer, and a
/// cursor. `ex_mode` distinguishes the `gQ` Ex namespace (stays open, re-prompting after each `<CR>`)
/// from a one-shot `:`/`/` line. History index / wildmenu / incsearch UX are deferred (acceptance #3).
struct CmdLine {
    /// `:` (ex) or `/` (search) — also the glyph the status line shows.
    prefix: char,
    /// The text typed so far. Owned HERE, never on the frontend.
    buffer: String,
    /// Insertion point as a char index. MVP edits append/backspace at the end (mid-line editing is the
    /// deferred full line-editor); the field exists because the namespace owns the cursor (acceptance #2).
    cursor: usize,
    /// `gQ` Ex mode: `<CR>` executes AND re-opens the line; `:visual`/`:vi`/empty exits.
    ex_mode: bool,
}

impl InputEngine {
    #[must_use]
    pub fn new() -> InputEngine {
        InputEngine {
            profile: VimProfile::new(),
            count: 0,
            op: None,
            awaiting: Awaiting::Nothing,
            last_find: None,
            last_search: None,
            last_change: None,
            recording: None,
            pending_record_register: None,
            pending_search: None,
            forced_wise: None,
            insert_one_shot: false,
            insert_ctrl_g: false,
            cmdline: None,
        }
    }

    /// The active command-line as `(prefix, text, cursor)` for the frontend to render — `None` when
    /// not on the command line. The frontend reads this instead of owning any line buffer (F-026).
    #[must_use]
    pub fn cmdline(&self) -> Option<(char, &str, usize)> {
        self.cmdline
            .as_ref()
            .map(|c| (c.prefix, c.buffer.as_str(), c.cursor))
    }

    /// Open the command-line namespace with `prefix` (`:`/`/`), optionally as `gQ` Ex mode.
    fn open_cmdline(&mut self, prefix: char, ex_mode: bool) {
        self.cmdline = Some(CmdLine {
            prefix,
            buffer: String::new(),
            cursor: 0,
            ex_mode,
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
                let text = std::mem::take(&mut cl.buffer);
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

    /// Clear the transient command state (count, operator, key-expectation). Sticky repeat state survives.
    /// Every non-`Pending` outcome runs through here, so no partial sequence ever leaks into the next command.
    fn reset(&mut self) {
        self.count = 0;
        self.op = None;
        self.awaiting = Awaiting::Nothing;
        self.forced_wise = None;
        // A `CTRL-O` one-shot is consumed the instant its single Normal command completes (every
        // completion runs through here), returning the engine to plain Insert routing.
        self.insert_one_shot = false;
    }

    fn mcount(&self) -> u32 {
        self.count.max(1)
    }

    /// Emit `m` — an operator command if one is armed, else a bare move — then clear the transient state.
    fn motion(&mut self, m: Motion) -> Feed {
        let cmd = match self.op {
            Some(OpPending { op, count }) => {
                let total = count.max(1) * self.mcount();
                // A forced wise (`dvj`/`dVe`) resolves into `OpForced`; the `cw`->`ce` rewrite is a plain-
                // change nicety that does not apply to the (rare) forced-change form.
                if let Some(wise) = self.forced_wise {
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
        if let Some(OpPending { op: armed, count }) = self.op {
            if armed == op {
                let n = count.max(1);
                self.reset();
                return Feed::Cmd(linewise(n, Motion::Line));
            }
        }
        self.op = Some(OpPending {
            op,
            count: self.mcount(),
        });
        self.count = 0;
        Feed::Pending
    }

    /// Feed one key given the current mode. Resolves the key into an outcome, then folds that outcome into
    /// the dot-repeat record (so `.` can later replay the last change). The two steps are split so the
    /// resolution grammar stays untouched by the recording concern.
    pub fn feed(&mut self, key: KeyEvent, mode: Mode) -> Feed {
        // The command-line namespace owns the keystream while open (F-026); its typing is not a
        // dot-repeatable change, so it bypasses the recorder.
        if self.cmdline.is_some() {
            return self.feed_cmdline(key);
        }
        let out = self.feed_impl(key, mode);
        self.record(&out, mode);
        out
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
    fn feed_impl(&mut self, key: KeyEvent, mode: Mode) -> Feed {
        // The `CTRL-O` one-shot and the `CTRL-G` prefix are Insert-only transient state, armed and consumed
        // between two consecutive Insert keys. Real flows keep the mode in Insert across them; a key that
        // arrives in any other mode means the insert context is gone, so the flags are stale — drop them so
        // they can never leak into a Normal/Visual command.
        if mode != Mode::Insert {
            self.insert_one_shot = false;
            self.insert_ctrl_g = false;
        }
        // Insert resolves through its LAYER, not through an early return ahead of everything else.
        // The bindings and the `open/insert` policy both live in `VimProfile`, so the namespace is
        // addressable in its own right (KL-OBL-1) and its policy is declared (KL-OBL-2).
        //
        // Two multi-key insert sequences are handled BEFORE the layer: `CTRL-O` (arm a one-shot Normal
        // command, then fall through to the Normal grammar for the rest of this and following keys until it
        // completes) and `CTRL-G u` (undo-break). A `CTRL-O` already in flight (`insert_one_shot`) skips the
        // insert branch entirely so the pending Normal command keeps resolving.
        if mode == Mode::Insert && !self.insert_one_shot {
            let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
            // `CTRL-G` prefix: consume the second key. `u` (or `U`) breaks the undo group; anything else
            // aborts the prefix without inserting (Vim beeps). Checked before the layer so the printable
            // path never sees the prefixed key.
            if self.insert_ctrl_g {
                self.insert_ctrl_g = false;
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
            if ctrl && key.code == KeyCode::Char('o') {
                // Arm the one-shot: the NEXT keys resolve through the Normal grammar; on completion
                // `reset()` disarms it and Insert routing resumes. Core mode stays Insert throughout.
                // Reset first so the one-shot begins from a clean count/operator/awaiting state.
                self.reset();
                self.insert_one_shot = true;
                return Feed::Pending;
            }
            if ctrl && key.code == KeyCode::Char('g') {
                self.reset();
                self.insert_ctrl_g = true;
                return Feed::Pending;
            }
            if let Resolved::Bound { value, .. } = self.profile.stack(Ns::Insert).resolve(&key.code)
            {
                let cmd = value.clone();
                self.reset();
                return Feed::Cmd(cmd);
            }
            return self.unmatched(Ns::Insert, key);
        }
        // Replace mode (`R`): overwrite policy. A printable key overwrites (or appends at EOL); `<BS>`
        // restores; `<Esc>` leaves. It is its own mode, not the Insert layer, because its unmatched-key
        // policy (overwrite) differs — the same open/policy framing as Insert vs Select.
        if mode == Mode::Replace {
            return match key.code {
                KeyCode::Esc => self.action(Command::EnterNormal),
                KeyCode::Backspace => self.action(Command::ReplaceBackspace),
                KeyCode::Enter => self.action(Command::InsertNewline),
                KeyCode::Char(c) => self.action(Command::ReplaceType(c)),
                _ => Feed::Ignored,
            };
        }
        // Virtual Replace mode (`gR`): same policy as Replace but the type key is tab-aware; `<BS>` shares
        // the Replace restore stack.
        if mode == Mode::VirtualReplace {
            return match key.code {
                KeyCode::Esc => self.action(Command::EnterNormal),
                KeyCode::Backspace => self.action(Command::ReplaceBackspace),
                KeyCode::Enter => self.action(Command::InsertNewline),
                KeyCode::Char(c) => self.action(Command::VirtualReplaceType(c)),
                _ => Feed::Ignored,
            };
        }
        // --- Top-priority tier: a one-shot key-expectation resolves before any base-key handling. ---
        match self.awaiting {
            Awaiting::FindTarget { forward, till } => {
                self.awaiting = Awaiting::Nothing;
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
                self.awaiting = Awaiting::Nothing;
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
                self.awaiting = Awaiting::Nothing;
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
                self.awaiting = Awaiting::Nothing;
                return match key.code {
                    // The count accumulated before `r` is still live (the `r` arm did not reset it).
                    KeyCode::Char(c) => self.action(Command::ReplaceChar(self.mcount(), c)),
                    // A pending construct is in flight, so this is `closed/abort` — the policy
                    // that distinguishes operator-pending from Normal (VS-OBL-3).
                    _ => self.unmatched(Ns::OperatorPending, key),
                };
            }
            Awaiting::RegisterSelect => {
                self.awaiting = Awaiting::Nothing;
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
                self.awaiting = Awaiting::Nothing;
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
                self.awaiting = Awaiting::FindTarget {
                    forward: true,
                    till: false,
                };
                return Feed::Pending;
            }
            KeyCode::Char('F') => {
                self.awaiting = Awaiting::FindTarget {
                    forward: false,
                    till: false,
                };
                return Feed::Pending;
            }
            KeyCode::Char('t') => {
                self.awaiting = Awaiting::FindTarget {
                    forward: true,
                    till: true,
                };
                return Feed::Pending;
            }
            KeyCode::Char('T') => {
                self.awaiting = Awaiting::FindTarget {
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
                self.awaiting = Awaiting::GSecond;
                return Feed::Pending;
            }
            KeyCode::Char('G') => {
                return if self.count > 0 {
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
                self.awaiting = Awaiting::RegisterSelect;
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
                    self.awaiting = Awaiting::TextObjectChar { inner: true };
                    return Feed::Pending;
                }
                KeyCode::Char('a') => {
                    self.awaiting = Awaiting::TextObjectChar { inner: false };
                    return Feed::Pending;
                }
                // Count digits and motions extend the selection; an unmatched key hits the namespace's
                // own policy — `closed/ignore` for Visual, `open/replace-selection` for Select.
                KeyCode::Char('1'..='9') => {}
                KeyCode::Char('0') if self.count > 0 => {}
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
                self.count = self.count.saturating_mul(10) + (d as u32 - '0' as u32);
                Feed::Pending
            }
            KeyCode::Char('0') if self.count > 0 => {
                self.count = self.count.saturating_mul(10);
                Feed::Pending
            }
            KeyCode::Char('0') => self.motion(Motion::LineStart),
            code if motion_key(code).is_some() => {
                self.motion(motion_key(code).expect("guarded by is_some"))
            }
            // With an operator armed, `v`/`V`/`CTRL-V` FORCE the next motion's wise (Vim `o_v`/`o_V`/
            // `o_CTRL-V`): `dvj`, `dVe`, `d<C-v>j`. They stay operator-pending (the motion still follows);
            // `motion` emits `OpForced`. Bare (no operator) they enter Visual/Visual-line/Visual-block.
            KeyCode::Char('v') if self.op.is_some() && ctrl => {
                self.forced_wise = Some(ForcedWise::Blockwise);
                Feed::Pending
            }
            KeyCode::Char('v') if self.op.is_some() => {
                self.forced_wise = Some(ForcedWise::Charwise);
                Feed::Pending
            }
            KeyCode::Char('V') if self.op.is_some() => {
                self.forced_wise = Some(ForcedWise::Linewise);
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
                self.awaiting = Awaiting::ShiftSecond { right: true };
                Feed::Pending
            }
            KeyCode::Char('<') => {
                self.awaiting = Awaiting::ShiftSecond { right: false };
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
            KeyCode::Char('i') if self.op.is_some() => {
                self.awaiting = Awaiting::TextObjectChar { inner: true };
                Feed::Pending
            }
            KeyCode::Char('a') if self.op.is_some() => {
                self.awaiting = Awaiting::TextObjectChar { inner: false };
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
                self.awaiting = Awaiting::ReplaceChar;
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
                    let count = (self.count > 0).then_some(self.count);
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
                let op = match self.op {
                    Some(OpPending { op, .. }) => match op {
                        Op::Delete => SearchOp::Delete,
                        Op::Change => SearchOp::Change,
                        Op::Yank => SearchOp::Yank,
                    },
                    None => SearchOp::Move,
                };
                let count = match self.op {
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
}

impl Default for InputEngine {
    /// Hand-written, NOT derived. A derived `Default` would build an empty layer set, every
    /// `resolve` would return `NoLayer`, and the policies would be silently disabled — the exact
    /// class of invisible regression the layer model exists to prevent.
    fn default() -> InputEngine {
        InputEngine::new()
    }
}

/// A parsed ex command (the `:` line).
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Ex {
    Save,
    Quit,
    SaveQuit,
    SaveTrace(String),
    Unknown(String),
}

/// Parse the text typed after `:` (without the leading colon).
#[must_use]
pub fn parse_ex(line: &str) -> Ex {
    let line = line.trim();
    match line {
        "w" => Ex::Save,
        "q" | "q!" => Ex::Quit,
        "wq" | "x" => Ex::SaveQuit,
        _ => match line.strip_prefix("trace save") {
            Some(rest) => Ex::SaveTrace(rest.trim().to_string()),
            None => Ex::Unknown(line.to_string()),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(super) fn k(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }
    pub(super) fn feed(seq: &str) -> Feed {
        let mut e = InputEngine::new();
        let mut last = Feed::Ignored;
        for c in seq.chars() {
            last = e.feed(k(c), Mode::Normal);
        }
        last
    }

    #[test]
    fn bare_motions_and_counts() {
        assert_eq!(feed("w"), Feed::Cmd(Command::Move(1, Motion::WordFwd)));
        assert_eq!(feed("3w"), Feed::Cmd(Command::Move(3, Motion::WordFwd)));
        assert_eq!(feed("l"), Feed::Cmd(Command::Move(1, Motion::Right)));
    }

    #[test]
    fn operators_with_counts() {
        assert_eq!(feed("dw"), Feed::Cmd(Command::Delete(1, Motion::WordFwd)));
        assert_eq!(feed("d2w"), Feed::Cmd(Command::Delete(2, Motion::WordFwd)));
        assert_eq!(feed("2dw"), Feed::Cmd(Command::Delete(2, Motion::WordFwd)));
        assert_eq!(feed("2d3w"), Feed::Cmd(Command::Delete(6, Motion::WordFwd)));
    }

    #[test]
    fn doubled_operator_is_linewise() {
        assert_eq!(feed("dd"), Feed::Cmd(Command::Delete(1, Motion::Line)));
        assert_eq!(feed("2dd"), Feed::Cmd(Command::Delete(2, Motion::Line)));
        assert_eq!(feed("cc"), Feed::Cmd(Command::Change(1, Motion::Line)));
    }

    #[test]
    fn forced_wise_after_operator() {
        // `v`/`V` after an operator FORCE the next motion's wise (Vim o_v/o_V) → OpForced.
        assert_eq!(
            feed("dvj"),
            Feed::Cmd(Command::OpForced {
                op: OpKind::Delete,
                count: 1,
                motion: Motion::Down,
                wise: ForcedWise::Charwise,
            })
        );
        assert_eq!(
            feed("dVe"),
            Feed::Cmd(Command::OpForced {
                op: OpKind::Delete,
                count: 1,
                motion: Motion::WordEnd,
                wise: ForcedWise::Linewise,
            })
        );
        // Count still multiplies through the forced form (`y2Vj`).
        assert_eq!(
            feed("y2Vj"),
            Feed::Cmd(Command::OpForced {
                op: OpKind::Yank,
                count: 2,
                motion: Motion::Down,
                wise: ForcedWise::Linewise,
            })
        );
    }

    #[test]
    fn bare_v_still_enters_visual() {
        // Without an operator armed, `v`/`V` enter Visual as before — the force only applies operator-pending.
        assert_eq!(
            feed("v"),
            Feed::Cmd(Command::EnterVisual {
                kind: SelectKind::Charwise
            })
        );
        assert_eq!(
            feed("V"),
            Feed::Cmd(Command::EnterVisual {
                kind: SelectKind::Linewise
            })
        );
    }

    #[test]
    fn cw_is_ce() {
        assert_eq!(feed("cw"), Feed::Cmd(Command::Change(1, Motion::WordEnd)));
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    #[test]
    fn ctrl_v_enters_blockwise_visual() {
        let mut e = InputEngine::new();
        assert_eq!(
            e.feed(ctrl('v'), Mode::Normal),
            Feed::Cmd(Command::EnterVisual {
                kind: SelectKind::Blockwise
            })
        );
    }

    #[test]
    fn v_slash_capital_v_slash_ctrl_v_switch_shape_or_leave() {
        let mut e = InputEngine::new();
        // From charwise: CTRL-V → blockwise, V → linewise, v → leave (Normal).
        let vis = Mode::Visual {
            kind: SelectKind::Charwise,
        };
        assert_eq!(
            e.feed(ctrl('v'), vis),
            Feed::Cmd(Command::EnterVisual {
                kind: SelectKind::Blockwise
            })
        );
        assert_eq!(e.feed(k('v'), vis), Feed::Cmd(Command::EnterNormal));
        // From blockwise: CTRL-V leaves, v → charwise, V → linewise.
        let blk = Mode::Visual {
            kind: SelectKind::Blockwise,
        };
        assert_eq!(e.feed(ctrl('v'), blk), Feed::Cmd(Command::EnterNormal));
        assert_eq!(
            e.feed(k('v'), blk),
            Feed::Cmd(Command::EnterVisual {
                kind: SelectKind::Charwise
            })
        );
        assert_eq!(
            e.feed(k('V'), blk),
            Feed::Cmd(Command::EnterVisual {
                kind: SelectKind::Linewise
            })
        );
    }

    #[test]
    fn block_selection_operators_route_like_any_selection() {
        let mut e = InputEngine::new();
        let blk = Mode::Visual {
            kind: SelectKind::Blockwise,
        };
        assert_eq!(e.feed(k('d'), blk), Feed::Cmd(Command::DeleteSelection));
        assert_eq!(e.feed(k('y'), blk), Feed::Cmd(Command::YankSelection));
    }

    #[test]
    fn block_mode_i_a_c_arm_insert_replicate() {
        let mut e = InputEngine::new();
        let blk = Mode::Visual {
            kind: SelectKind::Blockwise,
        };
        assert_eq!(
            e.feed(KeyEvent::new(KeyCode::Char('I'), KeyModifiers::NONE), blk),
            Feed::Cmd(Command::BlockInsert(BlockInsertKind::Insert))
        );
        assert_eq!(
            e.feed(KeyEvent::new(KeyCode::Char('A'), KeyModifiers::NONE), blk),
            Feed::Cmd(Command::BlockInsert(BlockInsertKind::Append))
        );
        assert_eq!(
            e.feed(k('c'), blk),
            Feed::Cmd(Command::BlockInsert(BlockInsertKind::Change))
        );
        assert_eq!(
            e.feed(k('s'), blk),
            Feed::Cmd(Command::BlockInsert(BlockInsertKind::Change))
        );
    }

    #[test]
    fn charwise_c_is_still_a_plain_change_and_lowercase_i_is_a_text_object() {
        let mut e = InputEngine::new();
        let vis = Mode::Visual {
            kind: SelectKind::Charwise,
        };
        // In charwise/linewise, `c` is the ordinary selection change (not a block-insert).
        assert_eq!(e.feed(k('c'), vis), Feed::Cmd(Command::ChangeSelection));
        // Lowercase `i` begins a text object in every shape (awaits the object key).
        let blk = Mode::Visual {
            kind: SelectKind::Blockwise,
        };
        assert_eq!(e.feed(k('i'), blk), Feed::Pending);
    }

    #[test]
    fn ctrl_v_after_operator_forces_blockwise() {
        // `d<C-v>j`: CTRL-V operator-pending forces the next motion blockwise (Vim o_CTRL-V).
        let mut e = InputEngine::new();
        assert_eq!(e.feed(k('d'), Mode::Normal), Feed::Pending);
        assert_eq!(e.feed(ctrl('v'), Mode::Normal), Feed::Pending);
        assert_eq!(
            e.feed(k('j'), Mode::Normal),
            Feed::Cmd(Command::OpForced {
                op: OpKind::Delete,
                count: 1,
                motion: Motion::Down,
                wise: ForcedWise::Blockwise,
            })
        );
    }

    #[test]
    fn ctrl_o_runs_one_normal_command_then_returns_to_insert() {
        // In Insert, CTRL-O arms a one-shot (Pending); the next key resolves through the NORMAL grammar
        // (here `x` → DeleteUnder), then the engine returns to plain Insert routing.
        let mut e = InputEngine::new();
        assert_eq!(e.feed(ctrl('o'), Mode::Insert), Feed::Pending);
        assert_eq!(
            e.feed(k('x'), Mode::Insert),
            Feed::Cmd(Command::DeleteUnder(1))
        );
        // Disarmed: the next key is an inserted char again, not a Normal command.
        assert_eq!(
            e.feed(k('x'), Mode::Insert),
            Feed::Cmd(Command::InsertChar('x'))
        );
    }

    #[test]
    fn ctrl_o_spans_a_multi_key_normal_command() {
        // A one-shot survives the intermediate Pending keys of a multi-key command (`dw`), disarming only
        // when the command completes.
        let mut e = InputEngine::new();
        assert_eq!(e.feed(ctrl('o'), Mode::Insert), Feed::Pending);
        assert_eq!(e.feed(k('d'), Mode::Insert), Feed::Pending); // operator armed
        assert_eq!(
            e.feed(k('w'), Mode::Insert),
            Feed::Cmd(Command::Delete(1, Motion::WordFwd))
        );
        assert_eq!(
            e.feed(k('z'), Mode::Insert),
            Feed::Cmd(Command::InsertChar('z')),
            "back to Insert after the one-shot command completes"
        );
    }

    #[test]
    fn ctrl_g_u_breaks_undo_and_other_second_keys_abort() {
        // CTRL-G is a one-key prefix in Insert: `u` (or `U`) emits BreakUndo.
        let mut e = InputEngine::new();
        assert_eq!(e.feed(ctrl('g'), Mode::Insert), Feed::Pending);
        assert_eq!(e.feed(k('u'), Mode::Insert), Feed::Cmd(Command::BreakUndo));
        // A non-`u` second key aborts the prefix without inserting; Insert then resumes normally.
        assert_eq!(e.feed(ctrl('g'), Mode::Insert), Feed::Pending);
        assert_eq!(e.feed(k('x'), Mode::Insert), Feed::Ignored);
        assert_eq!(
            e.feed(k('y'), Mode::Insert),
            Feed::Cmd(Command::InsertChar('y'))
        );
    }

    pub(super) fn esc() -> KeyEvent {
        KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)
    }

    fn fc(ch: char, forward: bool, till: bool) -> Motion {
        Motion::FindChar { ch, forward, till }
    }

    #[test]
    fn char_search_bare_and_operator() {
        assert_eq!(
            feed("fx"),
            Feed::Cmd(Command::Move(1, fc('x', true, false)))
        );
        assert_eq!(feed("tx"), Feed::Cmd(Command::Move(1, fc('x', true, true))));
        assert_eq!(
            feed("Fx"),
            Feed::Cmd(Command::Move(1, fc('x', false, false)))
        );
        assert_eq!(
            feed("Tx"),
            Feed::Cmd(Command::Move(1, fc('x', false, true)))
        );
        assert_eq!(
            feed("2fx"),
            Feed::Cmd(Command::Move(2, fc('x', true, false)))
        );
        // operator targets
        assert_eq!(
            feed("dtx"),
            Feed::Cmd(Command::Delete(1, fc('x', true, true)))
        );
        assert_eq!(
            feed("d2fx"),
            Feed::Cmd(Command::Delete(2, fc('x', true, false)))
        );
    }

    #[test]
    fn char_search_is_pending_until_the_target() {
        let mut e = InputEngine::new();
        assert_eq!(e.feed(k('f'), Mode::Normal), Feed::Pending);
        assert_eq!(
            e.feed(k('q'), Mode::Normal),
            Feed::Cmd(Command::Move(1, fc('q', true, false)))
        );
    }

    #[test]
    fn semicolon_repeats_comma_reverses() {
        let mut e = InputEngine::new();
        e.feed(k('f'), Mode::Normal);
        e.feed(k('x'), Mode::Normal); // last_find = (x, forward, not-till)
        assert_eq!(
            e.feed(k(';'), Mode::Normal),
            Feed::Cmd(Command::Move(1, fc('x', true, false))),
            "; repeats the last find"
        );
        assert_eq!(
            e.feed(k(','), Mode::Normal),
            Feed::Cmd(Command::Move(1, fc('x', false, false))),
            ", repeats reversed"
        );
    }

    #[test]
    fn line_jumps() {
        assert_eq!(feed("gg"), Feed::Cmd(Command::Move(1, Motion::GotoLine)));
        assert_eq!(feed("5gg"), Feed::Cmd(Command::Move(5, Motion::GotoLine)));
        assert_eq!(feed("G"), Feed::Cmd(Command::Move(1, Motion::LastLine)));
        assert_eq!(feed("5G"), Feed::Cmd(Command::Move(5, Motion::GotoLine)));
        // operator + line jump is linewise
        assert_eq!(feed("dG"), Feed::Cmd(Command::Delete(1, Motion::LastLine)));
        assert_eq!(feed("dgg"), Feed::Cmd(Command::Delete(1, Motion::GotoLine)));
    }

    #[test]
    fn single_key_edits() {
        // `r` is pending until the replacement char; ctrl-r is still redo.
        let mut e = InputEngine::new();
        assert_eq!(e.feed(k('r'), Mode::Normal), Feed::Pending);
        assert_eq!(
            e.feed(k('z'), Mode::Normal),
            Feed::Cmd(Command::ReplaceChar(1, 'z'))
        );
        assert_eq!(feed("x"), Feed::Cmd(Command::DeleteUnder(1)));
        assert_eq!(feed("~"), Feed::Cmd(Command::ToggleCase(1)));
        assert_eq!(feed("J"), Feed::Cmd(Command::JoinLines));
        // Counts multiply the single-key actions (Vim `3x` / `3~` / `3rz`).
        assert_eq!(feed("3x"), Feed::Cmd(Command::DeleteUnder(3)));
        assert_eq!(feed("3~"), Feed::Cmd(Command::ToggleCase(3)));
        let mut e = InputEngine::new();
        assert_eq!(e.feed(k('3'), Mode::Normal), Feed::Pending);
        assert_eq!(e.feed(k('r'), Mode::Normal), Feed::Pending);
        assert_eq!(
            e.feed(k('z'), Mode::Normal),
            Feed::Cmd(Command::ReplaceChar(3, 'z'))
        );
        let mut e = InputEngine::new();
        assert_eq!(
            e.feed(
                KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL),
                Mode::Normal
            ),
            Feed::Cmd(Command::Redo)
        );
    }

    #[test]
    fn shift_operators_doubled_and_counted() {
        // `>>` / `<<` are the doubled linewise forms; the count before them is the line count.
        assert_eq!(feed(">>"), Feed::Cmd(Command::ShiftRight(1)));
        assert_eq!(feed("<<"), Feed::Cmd(Command::ShiftLeft(1)));
        assert_eq!(feed("3>>"), Feed::Cmd(Command::ShiftRight(3)));
        assert_eq!(feed("2<<"), Feed::Cmd(Command::ShiftLeft(2)));
    }

    #[test]
    fn lone_shift_is_pending_then_aborts_on_mismatch() {
        let mut e = InputEngine::new();
        assert_eq!(e.feed(k('>'), Mode::Normal), Feed::Pending);
        // A mismatched second bracket aborts cleanly (operator-pending), leaking no state.
        assert_eq!(e.feed(k('<'), Mode::Normal), Feed::Ignored);
        assert!(e.op.is_none() && e.awaiting == Awaiting::Nothing && e.count == 0);
    }

    #[test]
    fn insert_entry_keys() {
        assert_eq!(feed("o"), Feed::Cmd(Command::OpenBelow));
        assert_eq!(feed("O"), Feed::Cmd(Command::OpenAbove));
        assert_eq!(feed("A"), Feed::Cmd(Command::AppendLineEnd));
        assert_eq!(feed("I"), Feed::Cmd(Command::InsertLineStart));
    }

    #[test]
    fn big_word_motions_and_cw_is_ce() {
        assert_eq!(feed("W"), Feed::Cmd(Command::Move(1, Motion::BigWordFwd)));
        assert_eq!(feed("B"), Feed::Cmd(Command::Move(1, Motion::BigWordBack)));
        assert_eq!(
            feed("dE"),
            Feed::Cmd(Command::Delete(1, Motion::BigWordEnd))
        );
        // `cw`/`cW` behave like `ce`/`cE`.
        assert_eq!(feed("cw"), Feed::Cmd(Command::Change(1, Motion::WordEnd)));
        assert_eq!(
            feed("cW"),
            Feed::Cmd(Command::Change(1, Motion::BigWordEnd))
        );
    }

    #[test]
    fn bracket_match() {
        assert_eq!(feed("%"), Feed::Cmd(Command::Move(1, Motion::MatchBracket)));
        assert_eq!(
            feed("d%"),
            Feed::Cmd(Command::Delete(1, Motion::MatchBracket))
        );
    }

    #[test]
    fn lone_g_is_pending_then_cancels_on_non_g() {
        let mut e = InputEngine::new();
        assert_eq!(e.feed(k('g'), Mode::Normal), Feed::Pending);
        assert_eq!(e.feed(k('z'), Mode::Normal), Feed::Ignored);
    }

    #[test]
    fn char_search_extends_visual() {
        let mut e = InputEngine::new();
        let vis = Mode::Visual {
            kind: SelectKind::Charwise,
        };
        assert_eq!(e.feed(k('f'), vis), Feed::Pending);
        assert_eq!(
            e.feed(k(')'), vis),
            Feed::Cmd(Command::Move(1, fc(')', true, false))),
            "f in Visual is a bare move that extends the selection"
        );
    }

    #[test]
    fn enters_visual_from_normal() {
        assert_eq!(
            feed("v"),
            Feed::Cmd(Command::EnterVisual {
                kind: SelectKind::Charwise
            })
        );
        assert_eq!(
            feed("V"),
            Feed::Cmd(Command::EnterVisual {
                kind: SelectKind::Linewise
            })
        );
    }

    #[test]
    fn visual_operators_act_on_the_selection() {
        let mut e = InputEngine::new();
        let vis = Mode::Visual {
            kind: SelectKind::Charwise,
        };
        assert_eq!(e.feed(k('d'), vis), Feed::Cmd(Command::DeleteSelection));
        assert_eq!(e.feed(k('y'), vis), Feed::Cmd(Command::YankSelection));
        assert_eq!(e.feed(k('c'), vis), Feed::Cmd(Command::ChangeSelection));
        assert_eq!(e.feed(k('x'), vis), Feed::Cmd(Command::DeleteSelection));
        assert_eq!(e.feed(esc(), vis), Feed::Cmd(Command::EnterNormal));
    }

    #[test]
    fn visual_o_swaps_selection_ends() {
        // In Visual/Select, `o` emits SwapSelectionEnds (in Normal it is OpenBelow).
        let mut e = InputEngine::new();
        assert_eq!(
            e.feed(
                k('o'),
                Mode::Visual {
                    kind: SelectKind::Charwise
                }
            ),
            Feed::Cmd(Command::SwapSelectionEnds)
        );
        assert_eq!(
            e.feed(
                k('o'),
                Mode::Select {
                    kind: SelectKind::Linewise
                }
            ),
            Feed::Cmd(Command::SwapSelectionEnds)
        );
        // Sanity: `o` in Normal is still OpenBelow.
        assert_eq!(feed("o"), Feed::Cmd(Command::OpenBelow));
    }

    #[test]
    fn visual_motion_extends_selection() {
        let mut e = InputEngine::new();
        let vis = Mode::Visual {
            kind: SelectKind::Charwise,
        };
        // A bare motion in Visual is a Move (no operator) — the frontend re-plans it against the anchor.
        assert_eq!(
            e.feed(k('l'), vis),
            Feed::Cmd(Command::Move(1, Motion::Right))
        );
        assert_eq!(
            e.feed(k('w'), vis),
            Feed::Cmd(Command::Move(1, Motion::WordFwd))
        );
    }

    #[test]
    fn visual_toggle_and_switch() {
        let mut e = InputEngine::new();
        // `v` in charwise Visual exits; `V` switches it to linewise.
        assert_eq!(
            e.feed(
                k('v'),
                Mode::Visual {
                    kind: SelectKind::Charwise
                }
            ),
            Feed::Cmd(Command::EnterNormal)
        );
        assert_eq!(
            e.feed(
                k('V'),
                Mode::Visual {
                    kind: SelectKind::Charwise
                }
            ),
            Feed::Cmd(Command::EnterVisual {
                kind: SelectKind::Linewise
            })
        );
        assert_eq!(
            e.feed(
                k('v'),
                Mode::Visual {
                    kind: SelectKind::Linewise
                }
            ),
            Feed::Cmd(Command::EnterVisual {
                kind: SelectKind::Charwise
            })
        );
    }

    fn ctrl_g() -> KeyEvent {
        KeyEvent::new(KeyCode::Char('g'), KeyModifiers::CONTROL)
    }

    #[test]
    fn ctrl_g_toggles_visual_and_select_both_ways() {
        let mut e = InputEngine::new();
        // Visual -> Select, carrying the charwise/linewise shape.
        assert_eq!(
            e.feed(
                ctrl_g(),
                Mode::Visual {
                    kind: SelectKind::Charwise
                }
            ),
            Feed::Cmd(Command::EnterSelect {
                kind: SelectKind::Charwise
            })
        );
        assert_eq!(
            e.feed(
                ctrl_g(),
                Mode::Visual {
                    kind: SelectKind::Linewise
                }
            ),
            Feed::Cmd(Command::EnterSelect {
                kind: SelectKind::Linewise
            })
        );
        // Select -> Visual, back again.
        assert_eq!(
            e.feed(
                ctrl_g(),
                Mode::Select {
                    kind: SelectKind::Charwise
                }
            ),
            Feed::Cmd(Command::EnterVisual {
                kind: SelectKind::Charwise
            })
        );
        assert_eq!(
            e.feed(
                ctrl_g(),
                Mode::Select {
                    kind: SelectKind::Linewise
                }
            ),
            Feed::Cmd(Command::EnterVisual {
                kind: SelectKind::Linewise
            })
        );
        // CTRL-G is inert in Normal (no selection to toggle); it is NOT the start of `gg`.
        assert_eq!(e.feed(ctrl_g(), Mode::Normal), Feed::Ignored);
    }

    #[test]
    fn printable_key_in_select_replaces_the_selection() {
        // A key that matches no motion/operator hits Select's `open/replace-selection` policy.
        let sel = Mode::Select {
            kind: SelectKind::Charwise,
        };
        let mut e = InputEngine::new();
        assert_eq!(
            e.feed(k('z'), sel),
            Feed::Cmd(Command::ReplaceSelection('z'))
        );
        // A non-printable unmatched key does nothing.
        assert_eq!(e.feed(esc(), sel), Feed::Cmd(Command::EnterNormal));
        let mut e = InputEngine::new();
        assert_eq!(
            e.feed(k('A'), sel),
            Feed::Cmd(Command::ReplaceSelection('A'))
        );
    }

    #[test]
    fn select_operators_and_motions_match_visual() {
        let sel = Mode::Select {
            kind: SelectKind::Charwise,
        };
        let mut e = InputEngine::new();
        // d/y/c act on the selection, exactly as in Visual.
        assert_eq!(e.feed(k('d'), sel), Feed::Cmd(Command::DeleteSelection));
        assert_eq!(e.feed(k('y'), sel), Feed::Cmd(Command::YankSelection));
        assert_eq!(e.feed(k('c'), sel), Feed::Cmd(Command::ChangeSelection));
        // A motion extends the selection (a bare Move; the frontend re-plans it against the anchor).
        assert_eq!(
            e.feed(k('l'), sel),
            Feed::Cmd(Command::Move(1, Motion::Right))
        );
        assert_eq!(
            e.feed(k('w'), sel),
            Feed::Cmd(Command::Move(1, Motion::WordFwd))
        );
        // Esc leaves the selection.
        assert_eq!(e.feed(esc(), sel), Feed::Cmd(Command::EnterNormal));
    }

    #[test]
    fn named_register_prefix_parses() {
        // `"a` is pending until the name, then emits SetRegister; the following op is unaffected.
        let mut e = InputEngine::new();
        assert_eq!(e.feed(k('"'), Mode::Normal), Feed::Pending);
        assert_eq!(
            e.feed(k('a'), Mode::Normal),
            Feed::Cmd(Command::SetRegister(Some('a')))
        );
        // `"a3yy` → the count typed after the register prefix still lands.
        let mut e = InputEngine::new();
        e.feed(k('"'), Mode::Normal);
        e.feed(k('a'), Mode::Normal);
        e.feed(k('3'), Mode::Normal);
        e.feed(k('y'), Mode::Normal);
        assert_eq!(
            e.feed(k('y'), Mode::Normal),
            Feed::Cmd(Command::Yank(3, Motion::Line)),
            "count after the register prefix still applies"
        );
        // Uppercase names (append) parse too.
        let mut e = InputEngine::new();
        e.feed(k('"'), Mode::Normal);
        assert_eq!(
            e.feed(k('A'), Mode::Normal),
            Feed::Cmd(Command::SetRegister(Some('A')))
        );
        // `"` works in Visual as well (Vim `"xy`).
        let mut e = InputEngine::new();
        let vis = Mode::Visual {
            kind: SelectKind::Charwise,
        };
        assert_eq!(e.feed(k('"'), vis), Feed::Pending);
        assert_eq!(
            e.feed(k('a'), vis),
            Feed::Cmd(Command::SetRegister(Some('a')))
        );
    }

    #[test]
    fn yank_operator_and_paste() {
        assert_eq!(feed("yw"), Feed::Cmd(Command::Yank(1, Motion::WordFwd)));
        assert_eq!(feed("y2w"), Feed::Cmd(Command::Yank(2, Motion::WordFwd)));
        assert_eq!(feed("yy"), Feed::Cmd(Command::Yank(1, Motion::Line)));
        assert_eq!(feed("2yy"), Feed::Cmd(Command::Yank(2, Motion::Line)));
        assert_eq!(feed("yiw"), Feed::Cmd(Command::Yank(1, Motion::InnerWord)));
        assert_eq!(
            feed("p"),
            Feed::Cmd(Command::Paste {
                after: true,
                count: 1
            })
        );
        assert_eq!(
            feed("P"),
            Feed::Cmd(Command::Paste {
                after: false,
                count: 1
            })
        );
    }

    #[test]
    fn pending_states() {
        let mut e = InputEngine::new();
        assert_eq!(e.feed(k('2'), Mode::Normal), Feed::Pending);
        assert_eq!(e.feed(k('d'), Mode::Normal), Feed::Pending);
        assert_eq!(
            e.feed(k('w'), Mode::Normal),
            Feed::Cmd(Command::Delete(2, Motion::WordFwd))
        );
    }

    #[test]
    fn insert_mode_and_ex() {
        let mut e = InputEngine::new();
        assert_eq!(
            e.feed(k('z'), Mode::Insert),
            Feed::Cmd(Command::InsertChar('z'))
        );
        assert_eq!(e.feed(k(':'), Mode::Normal), Feed::Pending);
        assert_eq!(parse_ex("wq"), Ex::SaveQuit);
        assert_eq!(
            parse_ex("trace save t.trace"),
            Ex::SaveTrace("t.trace".into())
        );
    }
}

/// Dot-repeat (`.`): the engine records the last change as a re-parameterizable [`ChangeIntent`] and
/// replays it — the operator/edit at the CURRENT cursor, plus any captured insert text (F-023).
#[cfg(test)]
mod dot_repeat_tests {
    use super::tests::{esc, k};
    use super::*;

    /// Feed a whole sequence, tracking the mode the way the frontend does (a completed command may change
    /// the mode, which the next key must see). Only the outcome matters here; we assert on the LAST feed.
    fn feed_modes(seq: &[KeyEvent]) -> (InputEngine, Feed) {
        let mut e = InputEngine::new();
        let mut mode = Mode::Normal;
        let mut last = Feed::Ignored;
        for key in seq {
            last = e.feed(*key, mode);
            // Track the handful of mode transitions dot-repeat capture depends on.
            match &last {
                Feed::Cmd(Command::EnterInsert)
                | Feed::Cmd(Command::EnterInsertAfter)
                | Feed::Cmd(Command::InsertLineStart)
                | Feed::Cmd(Command::AppendLineEnd)
                | Feed::Cmd(Command::OpenBelow)
                | Feed::Cmd(Command::OpenAbove)
                | Feed::Cmd(Command::Change(..)) => mode = Mode::Insert,
                Feed::Cmd(Command::EnterNormal) => mode = Mode::Normal,
                _ => {}
            }
        }
        (e, last)
    }

    #[test]
    fn dot_with_no_prior_change_is_a_clean_noop() {
        let mut e = InputEngine::new();
        assert_eq!(e.feed(k('.'), Mode::Normal), Feed::Ignored);
        assert!(e.last_change.is_none());
    }

    #[test]
    fn operator_change_replays_the_command_at_the_new_cursor() {
        // `dw` records Delete(1, WordFwd); `.` replays exactly that (motion re-run at the new cursor).
        let (_, last) = feed_modes(&[k('d'), k('w'), k('.')]);
        assert_eq!(
            last,
            Feed::Replay(vec![Command::Delete(1, Motion::WordFwd)])
        );
    }

    #[test]
    fn dot_does_not_overwrite_the_record_so_dot_dot_repeats() {
        // `dw..` — the second `.` replays the SAME recorded change, not "repeat of a repeat".
        let (_, last) = feed_modes(&[k('d'), k('w'), k('.'), k('.')]);
        assert_eq!(
            last,
            Feed::Replay(vec![Command::Delete(1, Motion::WordFwd)])
        );
    }

    #[test]
    fn counted_operator_is_recorded_with_its_count() {
        // `d2w` -> Delete(2, WordFwd); `.` repeats with the same count.
        let (_, last) = feed_modes(&[k('d'), k('2'), k('w'), k('.')]);
        assert_eq!(
            last,
            Feed::Replay(vec![Command::Delete(2, Motion::WordFwd)])
        );
    }

    #[test]
    fn single_key_edits_are_dot_repeatable() {
        assert_eq!(
            feed_modes(&[k('x'), k('.')]).1,
            Feed::Replay(vec![Command::DeleteUnder(1)])
        );
        assert_eq!(
            feed_modes(&[k('3'), k('x'), k('.')]).1,
            Feed::Replay(vec![Command::DeleteUnder(3)])
        );
        assert_eq!(
            feed_modes(&[k('>'), k('>'), k('.')]).1,
            Feed::Replay(vec![Command::ShiftRight(1)])
        );
        assert_eq!(
            feed_modes(&[k('d'), k('d'), k('.')]).1,
            Feed::Replay(vec![Command::Delete(1, Motion::Line)])
        );
    }

    #[test]
    fn n_dot_overrides_the_recorded_count() {
        // `3.` after `dw` replays with count 3 (Vim replaces, not multiplies).
        let (_, last) = feed_modes(&[k('d'), k('w'), k('3'), k('.')]);
        assert_eq!(
            last,
            Feed::Replay(vec![Command::Delete(3, Motion::WordFwd)])
        );
        // `2.` after `3x` replaces the 3 with 2.
        let (_, last) = feed_modes(&[k('3'), k('x'), k('2'), k('.')]);
        assert_eq!(last, Feed::Replay(vec![Command::DeleteUnder(2)]));
    }

    #[test]
    fn insert_change_replays_command_and_captured_text() {
        // `ciwFOO<Esc>` -> Change(1, InnerWord) + the inserted chars + the terminating EnterNormal.
        let (_, last) = feed_modes(&[
            k('c'),
            k('i'),
            k('w'),
            k('F'),
            k('O'),
            k('O'),
            esc(),
            k('.'),
        ]);
        assert_eq!(
            last,
            Feed::Replay(vec![
                Command::Change(1, Motion::InnerWord),
                Command::InsertChar('F'),
                Command::InsertChar('O'),
                Command::InsertChar('O'),
                Command::EnterNormal,
            ])
        );
    }

    #[test]
    fn append_insert_is_dot_repeatable_including_text() {
        // `A;<Esc>` then `.` replays AppendLineEnd + the ';' + Esc.
        let (_, last) = feed_modes(&[k('A'), k(';'), esc(), k('.')]);
        assert_eq!(
            last,
            Feed::Replay(vec![
                Command::AppendLineEnd,
                Command::InsertChar(';'),
                Command::EnterNormal,
            ])
        );
    }

    #[test]
    fn yank_is_not_dot_repeatable() {
        // Vim: `yw` is NOT a change; a following `.` has nothing to repeat.
        let mut e = InputEngine::new();
        e.feed(k('y'), Mode::Normal);
        e.feed(k('w'), Mode::Normal);
        assert!(e.last_change.is_none());
        assert_eq!(e.feed(k('.'), Mode::Normal), Feed::Ignored);
    }

    #[test]
    fn named_register_change_replays_with_its_register() {
        // `"ax` records DeleteUnder(1) carrying register a; `.` replays SetRegister(a) THEN the delete.
        let (_, last) = feed_modes(&[k('"'), k('a'), k('x'), k('.')]);
        assert_eq!(
            last,
            Feed::Replay(vec![
                Command::SetRegister(Some('a')),
                Command::DeleteUnder(1),
            ])
        );
    }

    #[test]
    fn unregistered_change_replays_without_a_register() {
        // A plain `x` after a stray-then-consumed register still replays bare (no leading SetRegister).
        let (_, last) = feed_modes(&[k('x'), k('.')]);
        assert_eq!(last, Feed::Replay(vec![Command::DeleteUnder(1)]));
    }

    #[test]
    fn motions_between_changes_do_not_clobber_the_record() {
        // `x` records; then a pure motion `w`; `.` still repeats the `x`.
        let (_, last) = feed_modes(&[k('x'), k('w'), k('.')]);
        assert_eq!(last, Feed::Replay(vec![Command::DeleteUnder(1)]));
    }
}

#[cfg(test)]
mod textobj_tests {
    use super::tests::*;
    use super::*;

    fn pair(open: char, close: char, around: bool) -> Motion {
        Motion::Pair {
            open,
            close,
            around,
        }
    }

    #[test]
    fn text_objects_compose() {
        assert_eq!(
            feed("diw"),
            Feed::Cmd(Command::Delete(1, Motion::InnerWord))
        );
        assert_eq!(
            feed("ciw"),
            Feed::Cmd(Command::Change(1, Motion::InnerWord))
        );
        assert_eq!(feed("daw"), Feed::Cmd(Command::Delete(1, Motion::AWord)));
    }

    #[test]
    fn word_and_bigword_objects() {
        assert_eq!(
            feed("diW"),
            Feed::Cmd(Command::Delete(1, Motion::InnerBigWord))
        );
        assert_eq!(feed("daW"), Feed::Cmd(Command::Delete(1, Motion::ABigWord)));
    }

    #[test]
    fn paragraph_and_sentence_objects() {
        assert_eq!(
            feed("yip"),
            Feed::Cmd(Command::Yank(1, Motion::InnerParagraph))
        );
        assert_eq!(
            feed("dap"),
            Feed::Cmd(Command::Delete(1, Motion::AParagraph))
        );
        assert_eq!(
            feed("dis"),
            Feed::Cmd(Command::Delete(1, Motion::InnerSentence))
        );
        assert_eq!(
            feed("das"),
            Feed::Cmd(Command::Delete(1, Motion::ASentence))
        );
    }

    #[test]
    fn delimiter_pair_objects_and_aliases() {
        // Inner vs around.
        assert_eq!(
            feed("di("),
            Feed::Cmd(Command::Delete(1, pair('(', ')', false)))
        );
        assert_eq!(
            feed("da("),
            Feed::Cmd(Command::Delete(1, pair('(', ')', true)))
        );
        // Closer and `b` alias to the same `()` object as the opener.
        assert_eq!(
            feed("di)"),
            Feed::Cmd(Command::Delete(1, pair('(', ')', false)))
        );
        assert_eq!(
            feed("dab"),
            Feed::Cmd(Command::Delete(1, pair('(', ')', true)))
        );
        // Braces: `{`/`}`/`B` collapse.
        assert_eq!(
            feed("ci{"),
            Feed::Cmd(Command::Change(1, pair('{', '}', false)))
        );
        assert_eq!(
            feed("daB"),
            Feed::Cmd(Command::Delete(1, pair('{', '}', true)))
        );
        // Brackets and angles.
        assert_eq!(
            feed("di["),
            Feed::Cmd(Command::Delete(1, pair('[', ']', false)))
        );
        assert_eq!(
            feed("da]"),
            Feed::Cmd(Command::Delete(1, pair('[', ']', true)))
        );
        assert_eq!(
            feed("di<"),
            Feed::Cmd(Command::Delete(1, pair('<', '>', false)))
        );
    }

    #[test]
    fn quote_objects() {
        assert_eq!(
            feed("da\""),
            Feed::Cmd(Command::Delete(
                1,
                Motion::Quote {
                    ch: '"',
                    around: true
                }
            ))
        );
        assert_eq!(
            feed("ci'"),
            Feed::Cmd(Command::Change(
                1,
                Motion::Quote {
                    ch: '\'',
                    around: false
                }
            ))
        );
        assert_eq!(
            feed("yi`"),
            Feed::Cmd(Command::Yank(
                1,
                Motion::Quote {
                    ch: '`',
                    around: false
                }
            ))
        );
    }

    #[test]
    fn text_object_extends_a_visual_selection() {
        // In Visual, `i`/`a` begin a text object; it completes as a bare `Move` the core turns into a span.
        let mut e = InputEngine::new();
        let vis = Mode::Visual {
            kind: SelectKind::Charwise,
        };
        assert_eq!(e.feed(k('i'), vis), Feed::Pending);
        assert_eq!(
            e.feed(k('w'), vis),
            Feed::Cmd(Command::Move(1, Motion::InnerWord))
        );
        let mut e = InputEngine::new();
        assert_eq!(e.feed(k('i'), vis), Feed::Pending);
        assert_eq!(
            e.feed(k('('), vis),
            Feed::Cmd(Command::Move(1, pair('(', ')', false)))
        );
    }

    #[test]
    fn tag_objects_are_deferred_and_abort_cleanly() {
        // `it`/`at` are carved out (no core syntax tree). The pending object aborts to a no-op, never panics.
        assert_eq!(feed("dit"), Feed::Ignored);
        assert_eq!(feed("dat"), Feed::Ignored);
        let mut e = InputEngine::new();
        assert_eq!(
            e.feed(k('v'), Mode::Normal),
            Feed::Cmd(Command::EnterVisual {
                kind: SelectKind::Charwise
            })
        );
        assert_eq!(
            e.feed(
                k('i'),
                Mode::Visual {
                    kind: SelectKind::Charwise
                }
            ),
            Feed::Pending
        );
        assert_eq!(
            e.feed(
                k('t'),
                Mode::Visual {
                    kind: SelectKind::Charwise
                }
            ),
            Feed::Ignored
        );
    }

    #[test]
    fn bare_i_still_enters_insert() {
        let mut e = InputEngine::new();
        assert_eq!(
            e.feed(k('i'), Mode::Normal),
            Feed::Cmd(Command::EnterInsert)
        );
    }
}

#[cfg(test)]
mod search_tests {
    use super::tests::k;
    use super::*;

    #[test]
    fn slash_opens_search_and_n_repeats() {
        let mut e = InputEngine::new();
        assert_eq!(e.feed(k('n'), Mode::Normal), Feed::Ignored); // no prior search yet
        assert_eq!(e.feed(k('/'), Mode::Normal), Feed::Pending);
        // Submitting the pattern yields a bare-move Search AND records it for `n`/`N`.
        assert_eq!(
            e.submit_search("foo".into()),
            Feed::Cmd(Command::Search {
                op: SearchOp::Move,
                count: 1,
                pattern: "foo".into()
            })
        );
        assert_eq!(
            e.feed(k('n'), Mode::Normal),
            Feed::Cmd(Command::SearchNext("foo".into()))
        );
        assert_eq!(
            e.feed(k('N'), Mode::Normal),
            Feed::Cmd(Command::SearchPrev("foo".into()))
        );
    }

    #[test]
    fn empty_search_pattern_is_inert() {
        let mut e = InputEngine::new();
        assert_eq!(e.feed(k('/'), Mode::Normal), Feed::Pending);
        assert_eq!(e.submit_search(String::new()), Feed::Ignored);
    }

    #[test]
    fn slash_clears_the_transient_axes_but_folds_the_operator_into_the_search() {
        // `/` is a MOTION: the transient count/op/awaiting axes are cleared (so nothing leaks into the
        // minibuffer), but an armed operator/count is CAPTURED for the search to consume — `d/pat`,
        // `2/pat`. This is the fix for the old behaviour that dropped the operator on `/`.
        let mut e = InputEngine::new();
        assert_eq!(e.feed(k('d'), Mode::Normal), Feed::Pending);
        assert_eq!(e.feed(k('/'), Mode::Normal), Feed::Pending);
        assert!(e.op.is_none() && e.awaiting == Awaiting::Nothing && e.count == 0);
        assert_eq!(
            e.submit_search("bar".into()),
            Feed::Cmd(Command::Search {
                op: SearchOp::Delete,
                count: 1,
                pattern: "bar".into()
            })
        );
    }

    #[test]
    fn count_before_slash_selects_the_nth_match() {
        let mut e = InputEngine::new();
        assert_eq!(e.feed(k('2'), Mode::Normal), Feed::Pending);
        assert_eq!(e.feed(k('/'), Mode::Normal), Feed::Pending);
        assert_eq!(
            e.submit_search("foo".into()),
            Feed::Cmd(Command::Search {
                op: SearchOp::Move,
                count: 2,
                pattern: "foo".into()
            })
        );
    }
}

/// Property tests over the input state machine: the hierarchy is explicit, so verify it has no holes —
/// no key sequence leaks a partial command, `awaiting` and the operator axis stay consistent, and `feed`
/// is deterministic. This is the mechanical guard against the "ad-hoc resolution order" class of bug.
#[cfg(test)]
mod state_machine_props {
    use super::*;
    use proptest::prelude::*;

    /// A key drawn from the meaningful command alphabet, plus arbitrary chars (find targets) and specials.
    fn any_key() -> impl Strategy<Value = KeyEvent> {
        let named = "0123456789hjklwbeWBEdcyiaoOAIxfFtT;,vVpPunN$/:gGrJ~%(){}[]<>\"'`sp"
            .chars()
            .collect::<Vec<_>>();
        prop_oneof![
            proptest::sample::select(named)
                .prop_map(|c| KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)),
            any::<char>().prop_map(|c| KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)),
            Just(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL)),
            Just(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::CONTROL)),
            Just(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            Just(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Just(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE)),
        ]
    }

    fn any_mode() -> impl Strategy<Value = Mode> {
        prop_oneof![
            Just(Mode::Normal),
            Just(Mode::Insert),
            Just(Mode::Replace),
            Just(Mode::Visual {
                kind: SelectKind::Charwise
            }),
            Just(Mode::Visual {
                kind: SelectKind::Linewise
            }),
            Just(Mode::Select {
                kind: SelectKind::Charwise
            }),
            Just(Mode::Select {
                kind: SelectKind::Linewise
            }),
        ]
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(256))]

        /// No key sequence ever leaves a partial command dangling: after any outcome that is not
        /// `Feed::Pending`, every transient axis (count, operator, key-expectation) is cleared. And a
        /// text-object expectation is only ever armed with an operator present. (Never panics — implicit.)
        #[test]
        fn no_pending_state_ever_leaks(steps in prop::collection::vec((any_key(), any_mode()), 0..80)) {
            let mut e = InputEngine::new();
            for (key, mode) in steps {
                let feed = e.feed(key, mode);
                // Orthogonal-axis invariant: a text object is only ever awaited with an operator armed
                // (`diw`) or from a selection mode (`viw`) — never bare in Normal. `mode` is the mode the
                // arming key was fed with, so this checks the arm site directly.
                if let Awaiting::TextObjectChar { .. } = e.awaiting {
                    let in_selection = matches!(mode, Mode::Visual { .. } | Mode::Select { .. });
                    prop_assert!(
                        e.op.is_some() || in_selection,
                        "text object awaited with neither an operator nor a selection"
                    );
                }
                if feed == Feed::Pending {
                    // A pending outcome must correspond to real accumulated state — including the two
                    // Insert-only prefixes (`CTRL-O` one-shot, `CTRL-G u`), which are pending too.
                    let has_state = e.count > 0
                        || e.op.is_some()
                        || e.awaiting != Awaiting::Nothing
                        || e.insert_one_shot
                        || e.insert_ctrl_g
                        || e.cmdline.is_some(); // an open command-line namespace is real pending state (F-026)
                    prop_assert!(has_state, "Feed::Pending but the engine is idle");
                } else {
                    prop_assert_eq!(e.count, 0, "count leaked after {:?}", feed);
                    prop_assert!(e.op.is_none(), "operator leaked after {:?}", feed);
                    prop_assert!(e.awaiting == Awaiting::Nothing, "key-expectation leaked after {:?}", feed);
                    prop_assert!(!e.insert_one_shot, "one-shot leaked after {:?}", feed);
                    prop_assert!(!e.insert_ctrl_g, "ctrl-g prefix leaked after {:?}", feed);
                }
            }
        }

        /// `feed` is a pure function of (state, key, mode): two engines fed the same sequence agree at
        /// every step. Determinism is what makes trace replay sound.
        #[test]
        fn feed_is_deterministic(steps in prop::collection::vec((any_key(), any_mode()), 0..40)) {
            let mut a = InputEngine::new();
            let mut b = InputEngine::new();
            for (key, mode) in steps {
                prop_assert_eq!(a.feed(key, mode), b.feed(key, mode));
            }
        }
    }
}

#[cfg(test)]
mod cmdline_tests {
    use super::tests::k;
    use super::*;

    fn special(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }
    fn enter() -> KeyEvent {
        special(KeyCode::Enter)
    }

    #[test]
    fn colon_opens_the_namespace_the_engine_owns_the_line_and_cr_executes() {
        // F-026 #1/#2: `:` enters the namespace; the engine (not the UI) owns the buffer; <CR> runs it.
        let mut e = InputEngine::new();
        assert_eq!(e.feed(k(':'), Mode::Normal), Feed::Pending);
        assert_eq!(e.cmdline(), Some((':', "", 0)));
        assert_eq!(e.feed(k('w'), Mode::Normal), Feed::Pending);
        assert_eq!(e.feed(k('q'), Mode::Normal), Feed::Pending);
        assert_eq!(
            e.cmdline(),
            Some((':', "wq", 2)),
            "the namespace owns the line buffer + cursor"
        );
        assert_eq!(e.feed(enter(), Mode::Normal), Feed::ExecuteEx("wq".into()));
        assert_eq!(e.cmdline(), None, "<CR> closes the one-shot command line");
    }

    #[test]
    fn slash_search_now_flows_entirely_through_the_engine() {
        // F-026: no ad-hoc UI buffer — `/`, the pattern chars, and <CR> all go through feed().
        let mut e = InputEngine::new();
        assert_eq!(e.feed(k('/'), Mode::Normal), Feed::Pending);
        for c in "foo".chars() {
            assert_eq!(e.feed(k(c), Mode::Normal), Feed::Pending);
        }
        assert_eq!(
            e.feed(enter(), Mode::Normal),
            Feed::Cmd(Command::Search {
                op: SearchOp::Move,
                count: 1,
                pattern: "foo".into()
            })
        );
        assert_eq!(e.cmdline(), None);
        // And the operator-fold still works because the buffer moved INTO the engine: `d/bar`.
        assert_eq!(e.feed(k('d'), Mode::Normal), Feed::Pending);
        assert_eq!(e.feed(k('/'), Mode::Normal), Feed::Pending);
        for c in "bar".chars() {
            e.feed(k(c), Mode::Normal);
        }
        assert_eq!(
            e.feed(enter(), Mode::Normal),
            Feed::Cmd(Command::Search {
                op: SearchOp::Delete,
                count: 1,
                pattern: "bar".into()
            })
        );
    }

    #[test]
    fn esc_aborts_the_command_line_without_executing() {
        let mut e = InputEngine::new();
        e.feed(k(':'), Mode::Normal);
        e.feed(k('x'), Mode::Normal);
        assert_eq!(e.feed(special(KeyCode::Esc), Mode::Normal), Feed::Ignored);
        assert_eq!(e.cmdline(), None, "<Esc> closes the line and runs nothing");
    }

    #[test]
    fn backspace_deletes_back_in_the_owned_buffer() {
        let mut e = InputEngine::new();
        e.feed(k(':'), Mode::Normal);
        e.feed(k('a'), Mode::Normal);
        e.feed(k('b'), Mode::Normal);
        e.feed(special(KeyCode::Backspace), Mode::Normal);
        assert_eq!(e.cmdline(), Some((':', "a", 1)));
    }

    #[test]
    fn gq_enters_ex_mode_reprompts_and_visual_exits() {
        // F-026 #3: `gQ` (only) enters Ex mode; <CR> runs AND re-opens; `:visual` leaves it.
        let mut e = InputEngine::new();
        e.feed(k('g'), Mode::Normal);
        assert_eq!(e.feed(k('Q'), Mode::Normal), Feed::Pending);
        assert_eq!(
            e.cmdline(),
            Some((':', "", 0)),
            "gQ opens the Ex command line"
        );
        for c in "set".chars() {
            e.feed(k(c), Mode::Normal);
        }
        assert_eq!(e.feed(enter(), Mode::Normal), Feed::ExecuteEx("set".into()));
        assert_eq!(
            e.cmdline(),
            Some((':', "", 0)),
            "Ex mode re-prompts after <CR>"
        );
        for c in "visual".chars() {
            e.feed(k(c), Mode::Normal);
        }
        assert_eq!(e.feed(enter(), Mode::Normal), Feed::Ignored);
        assert_eq!(e.cmdline(), None, "`:visual` exits Ex mode");
    }

    #[test]
    fn bare_q_is_not_an_ex_mode_key() {
        // At the pinned Neovim revision `Q` is replay-last-register, NOT Ex mode — only `gQ` opens it.
        let mut e = InputEngine::new();
        let out = e.feed(k('Q'), Mode::Normal);
        assert_ne!(
            out,
            Feed::Pending,
            "a bare Q must not open the Ex command line"
        );
        assert_eq!(e.cmdline(), None);
    }
}
