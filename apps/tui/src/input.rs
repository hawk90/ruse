//! The input engine: a small pending-state machine that folds keys into semantic [`Command`]s
//! (`d`, `2w`, `d3w`, `dd`, `cw`=`ce`), plus ex-command (`:…`) parsing. The trace records the resulting
//! commands, so re-keymapping never invalidates a corpus. Pure and unit-tested.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ruse_core::keymap::{Layer, LayerStack, Resolved, UnmatchedKey};
use ruse_core::{Command, Mode, Motion};

/// The outcome of feeding one key to the engine.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Feed {
    /// A completed command to apply.
    Cmd(Command),
    /// `:` — open the ex command line.
    OpenExLine,
    /// `/` — open the search line.
    OpenSearch,
    /// The key was consumed but the command is not complete yet (a count digit or a pending operator).
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
    /// After an operator then `i`/`a`: the next key is the text-object char (`w`). Only ever armed with an
    /// operator present (invariant, asserted in tests).
    TextObjectChar { inner: bool },
    /// After `g`: a second `g` completes `gg` (jump to the first line / `{count}gg`).
    GSecond,
    /// After `r`: the next key is the replacement char.
    ReplaceChar,
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

    /// Remember the pattern from a `/search` so `n`/`N` can repeat it.
    pub fn set_last_search(&mut self, pattern: String) {
        self.last_search = Some(pattern);
    }

    /// Clear the transient command state (count, operator, key-expectation). Sticky repeat state survives.
    /// Every non-`Pending` outcome runs through here, so no partial sequence ever leaks into the next command.
    fn reset(&mut self) {
        self.count = 0;
        self.op = None;
        self.awaiting = Awaiting::Nothing;
    }

    fn mcount(&self) -> u32 {
        self.count.max(1)
    }

    /// Emit `m` — an operator command if one is armed, else a bare move — then clear the transient state.
    fn motion(&mut self, m: Motion) -> Feed {
        let cmd = match self.op {
            Some(OpPending { op, count }) => {
                let total = count.max(1) * self.mcount();
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

    /// Feed one key given the current mode.
    pub fn feed(&mut self, key: KeyEvent, mode: Mode) -> Feed {
        // Insert resolves through its LAYER, not through an early return ahead of everything else.
        // The bindings and the `open/insert` policy both live in `VimProfile`, so the namespace is
        // addressable in its own right (KL-OBL-1) and its policy is declared (KL-OBL-2).
        if mode == Mode::Insert {
            if let Resolved::Bound { value, .. } = self.profile.stack(Ns::Insert).resolve(&key.code)
            {
                let cmd = value.clone();
                self.reset();
                return Feed::Cmd(cmd);
            }
            return self.unmatched(Ns::Insert, key);
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
                    KeyCode::Char('w') => self.motion(if inner {
                        Motion::InnerWord
                    } else {
                        Motion::AWord
                    }),
                    // A pending construct is in flight, so this is `closed/abort` — the policy
                    // that distinguishes operator-pending from Normal (VS-OBL-3).
                    _ => self.unmatched(Ns::OperatorPending, key),
                };
            }
            Awaiting::GSecond => {
                self.awaiting = Awaiting::Nothing;
                return match key.code {
                    KeyCode::Char('g') => self.motion(Motion::GotoLine),
                    // A pending construct is in flight, so this is `closed/abort` — the policy
                    // that distinguishes operator-pending from Normal (VS-OBL-3).
                    _ => self.unmatched(Ns::OperatorPending, key),
                };
            }
            Awaiting::ReplaceChar => {
                self.awaiting = Awaiting::Nothing;
                return match key.code {
                    KeyCode::Char(c) => self.action(Command::ReplaceChar(c)),
                    // A pending construct is in flight, so this is `closed/abort` — the policy
                    // that distinguishes operator-pending from Normal (VS-OBL-3).
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
                Mode::Visual { line } => self.action(Command::EnterSelect { line }),
                Mode::Select { line } => self.action(Command::EnterVisual { line }),
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
            _ => {}
        }
        // Visual and Select: the selection already exists, so operators act on it directly and motions
        // extend it. The two share every matched key here (identical selection state); they diverge ONLY
        // in the unmatched-key fallthrough — Visual ignores, Select replaces-and-inserts.
        //
        // DEFERRED (F-025 carve-out): `gv` (restore the PREVIOUS selection) is blocked on
        // CONCEPT-POSITION-HISTORY (C-ANCHOR) — a previous-selection store the engine does not have yet —
        // so it is intentionally unimplemented. `g` here only ever arms `gg`.
        if let Mode::Visual { line } | Mode::Select { line } = mode {
            match key.code {
                KeyCode::Esc => return self.action(Command::EnterNormal),
                // `v`/`V` toggle: same kind exits, the other switches charwise↔linewise.
                KeyCode::Char('v') => {
                    return self.action(if line {
                        Command::EnterVisual { line: false }
                    } else {
                        Command::EnterNormal
                    });
                }
                KeyCode::Char('V') => {
                    return self.action(if line {
                        Command::EnterNormal
                    } else {
                        Command::EnterVisual { line: true }
                    });
                }
                KeyCode::Char('d') | KeyCode::Char('x') => {
                    return self.action(Command::DeleteSelection)
                }
                KeyCode::Char('y') => return self.action(Command::YankSelection),
                KeyCode::Char('c') | KeyCode::Char('s') => {
                    return self.action(Command::ChangeSelection)
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
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
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
            KeyCode::Char('v') => self.action(Command::EnterVisual { line: false }),
            KeyCode::Char('V') => self.action(Command::EnterVisual { line: true }),
            KeyCode::Char('d') => self.operator(Op::Delete, Command::Delete),
            KeyCode::Char('c') => self.operator(Op::Change, Command::Change),
            KeyCode::Char('y') => self.operator(Op::Yank, Command::Yank),
            KeyCode::Char('p') => self.action(Command::Paste { after: true }),
            KeyCode::Char('P') => self.action(Command::Paste { after: false }),
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
            KeyCode::Char('x') => self.action(Command::DeleteUnder),
            KeyCode::Char('u') => self.action(Command::Undo),
            KeyCode::Char('r') if ctrl => self.action(Command::Redo),
            KeyCode::Char('r') => {
                self.awaiting = Awaiting::ReplaceChar;
                Feed::Pending
            }
            KeyCode::Char('~') => self.action(Command::ToggleCase),
            KeyCode::Char('J') => self.action(Command::JoinLines),
            KeyCode::Char('n') => match self.last_search.clone() {
                Some(p) => self.action(Command::SearchNext(p)),
                None => self.unmatched(Ns::Normal, key),
            },
            KeyCode::Char('N') => match self.last_search.clone() {
                Some(p) => self.action(Command::SearchPrev(p)),
                None => self.unmatched(Ns::Normal, key),
            },
            KeyCode::Char('/') => {
                self.reset();
                Feed::OpenSearch
            }
            KeyCode::Char(':') => {
                self.reset();
                Feed::OpenExLine
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
    fn cw_is_ce() {
        assert_eq!(feed("cw"), Feed::Cmd(Command::Change(1, Motion::WordEnd)));
    }

    fn esc() -> KeyEvent {
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
            Feed::Cmd(Command::ReplaceChar('z'))
        );
        assert_eq!(feed("~"), Feed::Cmd(Command::ToggleCase));
        assert_eq!(feed("J"), Feed::Cmd(Command::JoinLines));
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
        let vis = Mode::Visual { line: false };
        assert_eq!(e.feed(k('f'), vis), Feed::Pending);
        assert_eq!(
            e.feed(k(')'), vis),
            Feed::Cmd(Command::Move(1, fc(')', true, false))),
            "f in Visual is a bare move that extends the selection"
        );
    }

    #[test]
    fn enters_visual_from_normal() {
        assert_eq!(feed("v"), Feed::Cmd(Command::EnterVisual { line: false }));
        assert_eq!(feed("V"), Feed::Cmd(Command::EnterVisual { line: true }));
    }

    #[test]
    fn visual_operators_act_on_the_selection() {
        let mut e = InputEngine::new();
        let vis = Mode::Visual { line: false };
        assert_eq!(e.feed(k('d'), vis), Feed::Cmd(Command::DeleteSelection));
        assert_eq!(e.feed(k('y'), vis), Feed::Cmd(Command::YankSelection));
        assert_eq!(e.feed(k('c'), vis), Feed::Cmd(Command::ChangeSelection));
        assert_eq!(e.feed(k('x'), vis), Feed::Cmd(Command::DeleteSelection));
        assert_eq!(e.feed(esc(), vis), Feed::Cmd(Command::EnterNormal));
    }

    #[test]
    fn visual_motion_extends_selection() {
        let mut e = InputEngine::new();
        let vis = Mode::Visual { line: false };
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
            e.feed(k('v'), Mode::Visual { line: false }),
            Feed::Cmd(Command::EnterNormal)
        );
        assert_eq!(
            e.feed(k('V'), Mode::Visual { line: false }),
            Feed::Cmd(Command::EnterVisual { line: true })
        );
        assert_eq!(
            e.feed(k('v'), Mode::Visual { line: true }),
            Feed::Cmd(Command::EnterVisual { line: false })
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
            e.feed(ctrl_g(), Mode::Visual { line: false }),
            Feed::Cmd(Command::EnterSelect { line: false })
        );
        assert_eq!(
            e.feed(ctrl_g(), Mode::Visual { line: true }),
            Feed::Cmd(Command::EnterSelect { line: true })
        );
        // Select -> Visual, back again.
        assert_eq!(
            e.feed(ctrl_g(), Mode::Select { line: false }),
            Feed::Cmd(Command::EnterVisual { line: false })
        );
        assert_eq!(
            e.feed(ctrl_g(), Mode::Select { line: true }),
            Feed::Cmd(Command::EnterVisual { line: true })
        );
        // CTRL-G is inert in Normal (no selection to toggle); it is NOT the start of `gg`.
        assert_eq!(e.feed(ctrl_g(), Mode::Normal), Feed::Ignored);
    }

    #[test]
    fn printable_key_in_select_replaces_the_selection() {
        // A key that matches no motion/operator hits Select's `open/replace-selection` policy.
        let sel = Mode::Select { line: false };
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
        let sel = Mode::Select { line: false };
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
    fn yank_operator_and_paste() {
        assert_eq!(feed("yw"), Feed::Cmd(Command::Yank(1, Motion::WordFwd)));
        assert_eq!(feed("y2w"), Feed::Cmd(Command::Yank(2, Motion::WordFwd)));
        assert_eq!(feed("yy"), Feed::Cmd(Command::Yank(1, Motion::Line)));
        assert_eq!(feed("2yy"), Feed::Cmd(Command::Yank(2, Motion::Line)));
        assert_eq!(feed("yiw"), Feed::Cmd(Command::Yank(1, Motion::InnerWord)));
        assert_eq!(feed("p"), Feed::Cmd(Command::Paste { after: true }));
        assert_eq!(feed("P"), Feed::Cmd(Command::Paste { after: false }));
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
        assert_eq!(e.feed(k(':'), Mode::Normal), Feed::OpenExLine);
        assert_eq!(parse_ex("wq"), Ex::SaveQuit);
        assert_eq!(
            parse_ex("trace save t.trace"),
            Ex::SaveTrace("t.trace".into())
        );
    }
}

#[cfg(test)]
mod textobj_tests {
    use super::tests::*;
    use super::*;

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
        assert_eq!(e.feed(k('/'), Mode::Normal), Feed::OpenSearch);
        assert_eq!(e.feed(k('n'), Mode::Normal), Feed::Ignored); // no prior search yet
        e.set_last_search("foo".into());
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
    fn opening_a_command_line_clears_a_pending_operator() {
        // The `:`/`/` boundary to main.rs's cmd_line must not carry a half-typed operator across.
        let mut e = InputEngine::new();
        assert_eq!(e.feed(k('d'), Mode::Normal), Feed::Pending);
        assert_eq!(e.feed(k('/'), Mode::Normal), Feed::OpenSearch);
        assert!(e.op.is_none() && e.awaiting == Awaiting::Nothing && e.count == 0);
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
        let named = "0123456789hjklwbeWBEdcyiaoOAIxfFtT;,vVpPunN$/:gGrJ~%"
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
            Just(Mode::Visual { line: false }),
            Just(Mode::Visual { line: true }),
            Just(Mode::Select { line: false }),
            Just(Mode::Select { line: true }),
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
                // Orthogonal-axis invariant: TextObjectChar implies an armed operator.
                if let Awaiting::TextObjectChar { .. } = e.awaiting {
                    prop_assert!(e.op.is_some(), "text object awaited with no operator armed");
                }
                if feed == Feed::Pending {
                    // A pending outcome must correspond to real accumulated state.
                    let has_state = e.count > 0 || e.op.is_some() || e.awaiting != Awaiting::Nothing;
                    prop_assert!(has_state, "Feed::Pending but the engine is idle");
                } else {
                    prop_assert_eq!(e.count, 0, "count leaked after {:?}", feed);
                    prop_assert!(e.op.is_none(), "operator leaked after {:?}", feed);
                    prop_assert!(e.awaiting == Awaiting::Nothing, "key-expectation leaked after {:?}", feed);
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
