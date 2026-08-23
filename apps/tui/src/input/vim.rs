//! The Vim profile's static grammar: the keymap namespaces / layer tables (`Ns`, `VimProfile`) and the two
//! pure motion/text-object classifiers (`motion_key`, `text_object`). Split out of `input/mod.rs` as
//! self-contained grammar DATA — the engine methods that consult it (`feed`, the operator/motion dispatch)
//! stay in the engine core, keeping the Feed/Command/Mode coupling there (a deliberate PARTIAL split).

use crossterm::event::KeyCode;
use ruse_core::keymap::{Layer, LayerStack, UnmatchedKey};
use ruse_core::{Command, Motion};

/// The motion a movement key names, shared by Normal (bare move / operator) and Visual (extend selection).
/// `0` is deliberately excluded — it is a count digit unless the count is empty, so callers special-case it.
pub(crate) fn motion_key(code: KeyCode) -> Option<Motion> {
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
        // `+` / `<CR>` — count lines down to the first non-blank; `-` — count lines up; `_` — count-1 down
        // (`_` alone == `^`). All linewise under an operator (`d+`, `d-`, `d_`==`dd`).
        KeyCode::Char('+') | KeyCode::Enter => Motion::DownFirstNonBlank,
        KeyCode::Char('-') => Motion::UpFirstNonBlank,
        KeyCode::Char('_') => Motion::LineUnderscore,
        KeyCode::Char('|') => Motion::Column,
        KeyCode::Char('}') => Motion::ParagraphFwd,
        KeyCode::Char('{') => Motion::ParagraphBack,
        KeyCode::Char(')') => Motion::SentenceFwd,
        KeyCode::Char('(') => Motion::SentenceBack,
        _ => return None,
    })
}

/// The text object a char names after `i`/`a`, or `None` if the char is not a text-object selector. `inner`
/// picks `i…` (interior) vs `a…` (around). Aliases collapse per Vim: `b`≡`(`≡`)`, `B`≡`{`≡`}`, `]`≡`[`, etc.
///
/// `it`/`at` (tag objects) match an HTML/XML tag block by a nesting-aware BYTE scan in the core
/// (`motion::tag_span`) — no syntax tree is required, exactly as `i(`/`i"` scan for their delimiters.
pub(crate) fn text_object(ch: char, inner: bool) -> Option<Motion> {
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
        't' => Motion::Tag { around },
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
pub(crate) enum Ns {
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
    pub(crate) fn id(self) -> &'static str {
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
pub(crate) struct VimProfile {
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
    pub(crate) fn new() -> VimProfile {
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

    pub(crate) fn stack(&self, ns: Ns) -> &LayerStack<KeyCode, Command> {
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
    pub(crate) fn all() -> [Ns; 8] {
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

#[cfg(test)]
mod tests {
    use super::*;
    use ruse_core::keymap::UnmatchedKey;

    /// F-003 #3 (VS-OBL-4 / KL-OBL-1): each of the eight Vim map-mode namespaces is addressable in its
    /// own right, and F-003 #1 (KL-OBL-3): the Vim profile is depth-1 and SEALED — declared, not an
    /// accident. Asserted here rather than left as a comment so the property can never silently rot.
    #[test]
    fn all_eight_namespaces_are_addressable_depth_one_and_sealed() {
        let profile = VimProfile::new();
        for ns in VimProfile::all() {
            let stack = profile.stack(ns);
            assert_eq!(
                stack.depth(),
                1,
                "{ns:?} must be a single sealed layer (depth 1)"
            );
            let layer = stack
                .layer(ns.id())
                .expect("the namespace names its own layer");
            assert_eq!(
                layer.id(),
                ns.id(),
                "the layer is addressable by its namespace id"
            );
            assert!(layer.is_sealed(), "{ns:?} must be sealed (KL-OBL-3)");
        }
    }

    /// F-003 #4 (KL-OBL-2): every namespace declares its census unmatched-key policy explicitly —
    /// there is no engine-wide default. The values are vim-style.yaml's, derived from `map_mode`.
    #[test]
    fn each_namespace_declares_its_census_policy() {
        let profile = VimProfile::new();
        let expect = [
            (Ns::Normal, UnmatchedKey::Ignore),
            (Ns::OperatorPending, UnmatchedKey::Abort),
            (Ns::Insert, UnmatchedKey::Insert),
            (Ns::Cmdline, UnmatchedKey::Append),
            (Ns::Visual, UnmatchedKey::Ignore),
            (Ns::Select, UnmatchedKey::ReplaceSelection),
            (Ns::Terminal, UnmatchedKey::Forward),
            (Ns::Lang, UnmatchedKey::Translate),
        ];
        for (ns, policy) in expect {
            let layer = profile.stack(ns).layer(ns.id()).expect("layer exists");
            assert_eq!(layer.unmatched(), policy, "{ns:?} policy");
        }
    }

    /// F-003 #4: the five OPEN policies (insert/append/overwrite/replace-selection/forward) are all
    /// present across the declared namespaces and are distinct from the two CLOSED ones — the axis a
    /// shared `Feed::Ignored` fallthrough erases. `overwrite` rides the Replace namespace (insert
    /// family), the rest ride the eight.
    #[test]
    fn the_five_open_policies_are_declared_and_distinct() {
        let profile = VimProfile::new();
        let policy = |ns: Ns| profile.stack(ns).layer(ns.id()).unwrap().unmatched();
        let open = [
            policy(Ns::Insert),
            policy(Ns::Cmdline),
            policy(Ns::Replace),
            policy(Ns::Select),
            policy(Ns::Terminal),
        ];
        assert_eq!(
            open,
            [
                UnmatchedKey::Insert,
                UnmatchedKey::Append,
                UnmatchedKey::Overwrite,
                UnmatchedKey::ReplaceSelection,
                UnmatchedKey::Forward,
            ],
            "all five open policies are declared, each on a distinct namespace"
        );
        for p in open {
            assert!(p.is_open(), "{p:?} is an open policy");
        }
        assert!(!policy(Ns::Normal).is_open(), "Normal is closed/ignore");
        assert!(
            !policy(Ns::OperatorPending).is_open(),
            "Opr is closed/abort"
        );
    }
}
