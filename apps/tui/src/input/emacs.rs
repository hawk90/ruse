//! The Emacs profile's definitions (F-012 / D-045 / D-049): the nine-tier keymap table, the prefix-
//! argument and key data types, the `M-x` command registry, and the pure count-folding helpers. Split
//! out of `input/mod.rs` as self-contained definitions; the `InputEngine` dispatch methods
//! (`feed_emacs`, `feed_emacs_prefix`, `open_minibuffer`) stay in the engine core and reach these
//! through `pub(crate)` types / accessors.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ruse_core::keymap::{Layer, LayerStack, UnmatchedKey};
use ruse_core::{Command, Motion, WordCase};

use super::Feed;

/// An Emacs prefix argument mid-read (F-012 / D-049). `C-u` seeds it (default 4); a further `C-u`
/// multiplies by four; a digit turns it into an explicit decimal count. The engine hands the finished
/// value to the next command OPAQUELY — Vim would fold an equivalent count as a motion multiplier, Emacs
/// lets each command interpret it (motions multiply, `C-u C-SPC` would pop a mark, etc.).
#[derive(Clone, Copy)]
pub(crate) struct EmacsArg {
    /// The accumulated numeric value.
    value: i32,
    /// True once an explicit digit was typed: later digits append decimally rather than re-seeding, and a
    /// following `C-u` stops multiplying (the digits ARE the literal count the user asked for).
    has_digits: bool,
}

impl EmacsArg {
    /// A bare `C-u` — the universal argument's default value of four.
    pub(crate) fn ctrl_u() -> EmacsArg {
        EmacsArg {
            value: 4,
            has_digits: false,
        }
    }

    /// Another `C-u` while no digit has been typed multiplies the running value by four (`C-u C-u` = 16).
    pub(crate) fn times_four(self) -> EmacsArg {
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
    pub(crate) fn push_digit(self, d: u32) -> EmacsArg {
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
    pub(crate) fn count(self) -> u32 {
        self.value.max(0) as u32
    }
}

/// A resolved Emacs key: the code plus whether Control / Meta (Alt) / Shift were held. Emacs bindings are
/// fundamentally `modifier+key` (`C-f` ≠ `f`), so the keymap is keyed on this rather than a bare
/// [`KeyCode`] as the Vim namespaces are. Shift is tracked ONLY for non-character keys (`C-S-<backspace>`):
/// for a printable key Shift is already folded into the char (`Shift-2` = `@`), so tracking it there would
/// make a shifted printable miss its unshifted binding.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct EmacsKey {
    code: KeyCode,
    ctrl: bool,
    alt: bool,
    shift: bool,
}

impl EmacsKey {
    pub(crate) fn of(key: KeyEvent) -> EmacsKey {
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
pub(crate) enum EmacsBinding {
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
pub(crate) enum CountedCmd {
    DeleteForward, // C-d — Emacs delete-char: DeleteForward(count), no kill-ring write (D-026)
    Yank,          // C-y — Emacs yank: EmacsYank { count } (paste + set mark, D-051)
    Move(Motion),  // M-f / M-b — Move(count, motion)
    KillWord,      // M-d — Emacs kill-word: EmacsKillWord { count } (accumulating kill, D-051)
    BackwardKillWord, // M-DEL — Emacs backward-kill-word: EmacsBackwardKillWord { count } (prepend kill)
}

/// Fold the pending prefix count into a bound key's command. `Prefix` is handled by the caller (it mutates
/// engine state); it is inert here.
pub(crate) fn fold_emacs_count(binding: &EmacsBinding, count: u32) -> Feed {
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
pub(crate) enum Step {
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
pub(crate) struct EmacsProfile {
    pub(crate) map: LayerStack<EmacsKey, EmacsBinding>,
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
    pub(crate) fn new() -> EmacsProfile {
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
pub(crate) fn emacs_repeat(cmd: Command, count: u32) -> Feed {
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
