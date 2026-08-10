//! Enter/exit control sequences (F-010 acceptance #3).
//!
//! Whatever the probe turned ON at startup must be turned OFF on exit, in REVERSE order, or the
//! parent shell inherits a terminal left in kitty-keyboard / bracketed-paste / mouse mode — a
//! real, common corruption bug (architecture §6.1, "always pop/reset flags on exit"). [`enter`]
//! and [`exit`] are pure functions of the [`Ledger`], so the exact reversal is unit-testable and
//! the `Drop` in `main.rs` only has to write [`exit`]'s bytes.
//!
//! Only capabilities the ledger reports as ENABLED are pushed — a safe fallback (§6.3) emits
//! nothing, so a bare terminal is never sent sequences it did not answer to.

use super::ledger::{Capability, KeyEncoding, Ledger};

/// Kitty keyboard progressive-enhancement flags: `1` disambiguate escape codes + `2` report event
/// types = `3`. Matches neovim's `tui_set_key_encoding` (`CSI > 3 u`).
const KITTY_FLAGS: u8 = 3;

/// The sequences to PUSH on enter, in application order. Popped in reverse by [`exit`].
///
/// Order: keyboard encoding first (so the very next keypress is already disambiguated), then the
/// screen modes. Kitty uses the stack form `CSI > <flags> u` (paired with `CSI < u` on exit);
/// xterm modifyOtherKeys has no stack, so it is reset explicitly on exit.
pub fn enter(ledger: &Ledger) -> Vec<u8> {
    let mut out = Vec::new();
    match ledger.key_encoding() {
        KeyEncoding::Kitty => out.extend_from_slice(format!("\x1b[>{KITTY_FLAGS}u").as_bytes()),
        KeyEncoding::Xterm => out.extend_from_slice(b"\x1b[>4;2m"),
        KeyEncoding::Legacy => {}
    }
    if ledger.enabled(Capability::BracketedPaste) {
        out.extend_from_slice(b"\x1b[?2004h");
    }
    if ledger.enabled(Capability::SgrMouse) {
        // Button + any-motion tracking, encoded with SGR-1006 extended coordinates.
        out.extend_from_slice(b"\x1b[?1002h\x1b[?1003h\x1b[?1006h");
    }
    if ledger.enabled(Capability::ResizeEvents) {
        out.extend_from_slice(b"\x1b[?2048h");
    }
    // SynchronizedOutput is wrapped per-frame (BSU/ESU) by the renderer, not a persistent enter
    // mode, so it is deliberately not pushed here — enabling it globally would suppress no output.
    out
}

/// The sequences to POP on exit — the exact inverse of [`enter`], applied in REVERSE order, so the
/// terminal is handed back to the parent shell exactly as it was found.
pub fn exit(ledger: &Ledger) -> Vec<u8> {
    let mut out = Vec::new();
    if ledger.enabled(Capability::ResizeEvents) {
        out.extend_from_slice(b"\x1b[?2048l");
    }
    if ledger.enabled(Capability::SgrMouse) {
        out.extend_from_slice(b"\x1b[?1006l\x1b[?1003l\x1b[?1002l");
    }
    if ledger.enabled(Capability::BracketedPaste) {
        out.extend_from_slice(b"\x1b[?2004l");
    }
    match ledger.key_encoding() {
        KeyEncoding::Kitty => out.extend_from_slice(b"\x1b[<u"), // pop the pushed flags
        KeyEncoding::Xterm => out.extend_from_slice(b"\x1b[>4;0m"), // reset modifyOtherKeys
        KeyEncoding::Legacy => {}
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::caps::ledger::{CapValue, Confidence, Entry, Source};

    fn on(cap: Capability) -> (Capability, Entry) {
        (
            cap,
            Entry {
                value: CapValue::Bool(true),
                source: Source::Probed,
                confidence: Confidence::Confirmed,
            },
        )
    }

    #[test]
    fn safe_fallback_pushes_nothing() {
        let l = Ledger::with_defaults();
        assert!(enter(&l).is_empty());
        assert!(exit(&l).is_empty());
    }

    #[test]
    fn kitty_is_pushed_and_popped() {
        let mut l = Ledger::with_defaults();
        l.set_override(
            Capability::KeyEncoding,
            CapValue::Encoding(KeyEncoding::Kitty),
        );
        assert_eq!(enter(&l), b"\x1b[>3u");
        assert_eq!(exit(&l), b"\x1b[<u");
    }

    #[test]
    fn exit_reverses_enter_mode_order() {
        let mut l = Ledger::with_defaults();
        l.set_override(
            Capability::KeyEncoding,
            CapValue::Encoding(KeyEncoding::Kitty),
        );
        let (c, e) = on(Capability::BracketedPaste);
        l.record(c, e);
        let (c, e) = on(Capability::ResizeEvents);
        l.record(c, e);

        // Enter: encoding, then paste, then resize.
        assert_eq!(enter(&l), b"\x1b[>3u\x1b[?2004h\x1b[?2048h");
        // Exit: resize, then paste, then encoding — the exact reverse, no leftover modes.
        assert_eq!(exit(&l), b"\x1b[?2048l\x1b[?2004l\x1b[<u");
    }

    #[test]
    fn only_enabled_capabilities_are_emitted() {
        let mut l = Ledger::with_defaults();
        let (c, e) = on(Capability::BracketedPaste);
        l.record(c, e);
        // Mouse stays at its default (off) — must not appear in either direction.
        let entered = enter(&l);
        assert!(entered.windows(4).all(|w| w != b"1002"));
        assert_eq!(entered, b"\x1b[?2004h");
        assert_eq!(exit(&l), b"\x1b[?2004l");
    }
}
