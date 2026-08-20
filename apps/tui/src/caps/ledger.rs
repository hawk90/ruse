//! The capability ledger (F-010 acceptance #2).
//!
//! A terminal capability is never a bare `bool`: it carries WHERE the belief came from (`source`)
//! and HOW sure we are (`confidence`), so a low-confidence env guess can be overwritten by a
//! high-confidence DA1-fenced probe, and a **user override always wins** over both. This is the
//! anti-pattern architecture §6.3 names ("do not represent capability as just a few bools").
//!
//! Pure and terminal-free: [`Ledger`] is built by [`super::probe`] from a byte stream and read by
//! [`super::sequences`] to decide what to enable — neither step touches IO, so the whole model is
//! unit-testable without a terminal (the parity-harness discipline, see `lib.rs`).

use std::collections::BTreeMap;

/// The keyboard-encoding regime, mirroring the neovim `key_encoding` census surface
/// (`nvim.keyenc.{legacy,xterm,kitty}`). `Legacy` is the absence of both negotiated protocols.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum KeyEncoding {
    /// Legacy escape parser; ESC/Alt disambiguation needs the ~25–50 ms timeout (TERM-KBD-4).
    Legacy,
    /// xterm `modifyOtherKeys` / XTMODKEYS (`nvim.keyenc.xterm`, TERM-KBD-3).
    Xterm,
    /// Kitty keyboard protocol — disambiguates esc, reports event types (`nvim.keyenc.kitty`).
    Kitty,
}

/// The inline-graphics protocol a terminal supports, as a DEGRADATION LADDER (F-031 slice 3b / D-053):
/// higher = more preferred, so `graphics()` picking the max is "prefer Kitty, then Sixel, then iTerm2".
/// `None` is a terminal with no inline graphics (the placeholder rung of INV-CAP-DEGRADE).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum GraphicsProtocol {
    /// No inline image support — render the Unicode/placeholder rung.
    None,
    /// iTerm2 inline images (`OSC 1337`), env-hinted from `$TERM_PROGRAM`.
    ITerm2,
    /// Sixel (`DCS ... q`), advertised by a `4` in the DA1 attributes.
    Sixel,
    /// Kitty graphics protocol (`APC _G`), the most capable — env-hinted from `$TERM`/`$TERM_PROGRAM`.
    Kitty,
}

/// A capability ruse probes and negotiates. Each maps to a census id under
/// `FAM-TERMINAL-CAPABILITY` (spec/parity/inventory/neovim/{term_mode,key_encoding}.yaml).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Capability {
    /// Which keyboard-encoding regime is active (value: [`CapValue::Encoding`]).
    KeyEncoding,
    /// Bracketed paste, DEC private mode 2004 (`nvim.termmode.bracketedpaste`, TERM-PASTE-1).
    BracketedPaste,
    /// SGR-1006 mouse, modes 1006/1002/1003 as a unit (`nvim.termmode.mousesgrext`, TERM-MOUSE-1).
    SgrMouse,
    /// Synchronized output, mode 2026 (`nvim.termmode.synchronizedoutput`).
    SynchronizedOutput,
    /// In-band resize reporting, mode 2048 (`nvim.termmode.resizeevents`).
    ResizeEvents,
    /// Inline-graphics protocol for images (value: [`CapValue::Graphics`]) — F-031 slice 3b / D-053.
    InlineGraphics,
}

/// The value a capability holds: a plain on/off, the keyboard-encoding regime, or the graphics protocol.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CapValue {
    Bool(bool),
    Encoding(KeyEncoding),
    Graphics(GraphicsProtocol),
}

impl CapValue {
    /// True when the capability is usable — `on`, any negotiated (non-legacy) encoding, or any inline
    /// graphics protocol (non-`None`).
    pub fn is_enabled(self) -> bool {
        match self {
            CapValue::Bool(b) => b,
            CapValue::Encoding(e) => e != KeyEncoding::Legacy,
            CapValue::Graphics(g) => g != GraphicsProtocol::None,
        }
    }
}

/// Where a belief came from. Ordered by PRECEDENCE (low → high): a write only takes effect if its
/// source is at least as authoritative as the one already on record, so [`Source::UserOverride`]
/// can never be clobbered by a later probe.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Source {
    /// The zero-config safe default (architecture §6.3 "safe fallback").
    Default,
    /// Inferred from the environment (`$TERM`, `$COLORTERM`, `$TERM_PROGRAM`) — a hint, not truth.
    EnvHint,
    /// Confirmed by an active, DA1-fenced probe reply.
    Probed,
    /// Set explicitly by the user — always wins (architecture §6.3 "user override").
    UserOverride,
}

/// How much to trust a value. Independent of `source` (a probe can still be inconclusive).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Confidence {
    /// No evidence beyond the default.
    Unknown,
    /// A heuristic said so.
    Assumed,
    /// The terminal answered.
    Confirmed,
}

/// One ledger row.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Entry {
    pub value: CapValue,
    pub source: Source,
    pub confidence: Confidence,
}

/// The per-capability confidence ledger. Empty until seeded with [`Ledger::with_defaults`].
#[derive(Clone, Debug, Default)]
pub struct Ledger {
    rows: BTreeMap<Capability, Entry>,
}

impl Ledger {
    /// An empty ledger — every `get` returns `None`.
    pub fn new() -> Self {
        Ledger {
            rows: BTreeMap::new(),
        }
    }

    /// The safe-fallback baseline (architecture §6.3): everything off / legacy, `Source::Default`.
    /// A probe or user override upgrades these; nothing degrades below them.
    pub fn with_defaults() -> Self {
        let mut l = Ledger::new();
        let def = |v| Entry {
            value: v,
            source: Source::Default,
            confidence: Confidence::Unknown,
        };
        l.rows.insert(
            Capability::KeyEncoding,
            def(CapValue::Encoding(KeyEncoding::Legacy)),
        );
        l.rows
            .insert(Capability::BracketedPaste, def(CapValue::Bool(false)));
        l.rows
            .insert(Capability::SgrMouse, def(CapValue::Bool(false)));
        l.rows
            .insert(Capability::SynchronizedOutput, def(CapValue::Bool(false)));
        l.rows
            .insert(Capability::ResizeEvents, def(CapValue::Bool(false)));
        l.rows.insert(
            Capability::InlineGraphics,
            def(CapValue::Graphics(GraphicsProtocol::None)),
        );
        l
    }

    /// Record a belief. It takes effect only if `source` is at least as authoritative as what is
    /// already on record (`Source` ordering) — so a stale probe cannot overwrite a user override,
    /// and a re-probe at equal authority refreshes the value. Returns whether the row changed.
    pub fn record(&mut self, cap: Capability, entry: Entry) -> bool {
        match self.rows.get(&cap) {
            Some(cur) if entry.source < cur.source => false,
            _ => {
                self.rows.insert(cap, entry);
                true
            }
        }
    }

    /// A user override — the highest authority, always wins (acceptance #2).
    pub fn set_override(&mut self, cap: Capability, value: CapValue) {
        self.rows.insert(
            cap,
            Entry {
                value,
                source: Source::UserOverride,
                confidence: Confidence::Confirmed,
            },
        );
    }

    /// The current entry for a capability, if the ledger tracks it.
    pub fn get(&self, cap: Capability) -> Option<Entry> {
        self.rows.get(&cap).copied()
    }

    /// True when the capability is present and enabled.
    pub fn enabled(&self, cap: Capability) -> bool {
        self.get(cap).is_some_and(|e| e.value.is_enabled())
    }

    /// The active keyboard-encoding regime (defaults to `Legacy` if untracked).
    pub fn key_encoding(&self) -> KeyEncoding {
        match self.get(Capability::KeyEncoding).map(|e| e.value) {
            Some(CapValue::Encoding(e)) => e,
            _ => KeyEncoding::Legacy,
        }
    }

    /// The inline-graphics protocol to use (defaults to `None` if untracked) — F-031 slice 3b / D-053.
    pub fn graphics(&self) -> GraphicsProtocol {
        match self.get(Capability::InlineGraphics).map(|e| e.value) {
            Some(CapValue::Graphics(g)) => g,
            _ => GraphicsProtocol::None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn probed(v: CapValue) -> Entry {
        Entry {
            value: v,
            source: Source::Probed,
            confidence: Confidence::Confirmed,
        }
    }

    #[test]
    fn defaults_are_safe_fallback() {
        let l = Ledger::with_defaults();
        assert!(!l.enabled(Capability::BracketedPaste));
        assert_eq!(l.key_encoding(), KeyEncoding::Legacy);
        assert_eq!(l.get(Capability::SgrMouse).unwrap().source, Source::Default);
    }

    #[test]
    fn graphics_ladder_defaults_none_records_and_override_wins() {
        let mut l = Ledger::with_defaults();
        assert_eq!(l.graphics(), GraphicsProtocol::None);
        assert!(!l.enabled(Capability::InlineGraphics));
        // A probed Sixel upgrades the default.
        assert!(l.record(
            Capability::InlineGraphics,
            probed(CapValue::Graphics(GraphicsProtocol::Sixel)),
        ));
        assert_eq!(l.graphics(), GraphicsProtocol::Sixel);
        assert!(l.enabled(Capability::InlineGraphics));
        // A user override always wins (force graphics off).
        l.set_override(
            Capability::InlineGraphics,
            CapValue::Graphics(GraphicsProtocol::None),
        );
        assert_eq!(l.graphics(), GraphicsProtocol::None);
    }

    #[test]
    fn graphics_ladder_orders_by_preference() {
        // The enum order encodes D-053 preference: Kitty > Sixel > iTerm2 > None.
        assert!(GraphicsProtocol::Kitty > GraphicsProtocol::Sixel);
        assert!(GraphicsProtocol::Sixel > GraphicsProtocol::ITerm2);
        assert!(GraphicsProtocol::ITerm2 > GraphicsProtocol::None);
    }

    #[test]
    fn probe_upgrades_default() {
        let mut l = Ledger::with_defaults();
        assert!(l.record(Capability::BracketedPaste, probed(CapValue::Bool(true))));
        assert!(l.enabled(Capability::BracketedPaste));
        assert_eq!(
            l.get(Capability::BracketedPaste).unwrap().confidence,
            Confidence::Confirmed
        );
    }

    #[test]
    fn user_override_beats_a_later_probe() {
        let mut l = Ledger::with_defaults();
        l.set_override(Capability::SgrMouse, CapValue::Bool(false));
        // A probe that says the terminal DOES support SGR mouse must NOT flip a user's explicit off.
        assert!(!l.record(Capability::SgrMouse, probed(CapValue::Bool(true))));
        assert!(!l.enabled(Capability::SgrMouse));
        assert_eq!(
            l.get(Capability::SgrMouse).unwrap().source,
            Source::UserOverride
        );
    }

    #[test]
    fn probe_beats_env_hint_which_beats_default() {
        let mut l = Ledger::with_defaults();
        let hint = Entry {
            value: CapValue::Encoding(KeyEncoding::Xterm),
            source: Source::EnvHint,
            confidence: Confidence::Assumed,
        };
        assert!(l.record(Capability::KeyEncoding, hint));
        assert_eq!(l.key_encoding(), KeyEncoding::Xterm);
        // A DA1-fenced probe confirming kitty outranks the env hint.
        assert!(l.record(
            Capability::KeyEncoding,
            probed(CapValue::Encoding(KeyEncoding::Kitty))
        ));
        assert_eq!(l.key_encoding(), KeyEncoding::Kitty);
        // A stale env hint arriving afterwards cannot demote the probe.
        assert!(!l.record(Capability::KeyEncoding, hint));
        assert_eq!(l.key_encoding(), KeyEncoding::Kitty);
    }
}
