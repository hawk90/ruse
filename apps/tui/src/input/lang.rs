//! The Lang-Arg namespace (F-027, D-048 / RFC-0013): the `:lmap` table accessors and the pre-dispatch
//! translation stage `translate_lang` that `feed` runs first. Moved out of `input/mod.rs` (#1493).

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ruse_core::Mode;

use super::{Awaiting, InputEngine};

impl InputEngine {
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
    pub(super) fn lang_eligible(&self, mode: Mode) -> bool {
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
    pub(super) fn translate_lang(&self, key: KeyEvent, mode: Mode) -> KeyEvent {
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
}
