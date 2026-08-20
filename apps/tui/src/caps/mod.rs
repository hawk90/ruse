//! Terminal capability detection (F-010): a DA1-fenced active probe feeding a confidence ledger,
//! with a safe fallback and user override, and matching enter/exit sequences.
//!
//! Layers, all pure and terminal-free so they unit-test without a tty:
//!   * [`ledger`]    — the `{value, source, confidence}` model; user override always wins.
//!   * [`probe`]     — the outbound query batch + the inbound DA1-fenced reply parser.
//!   * [`sequences`] — what to push on enter and pop (in reverse) on exit.
//!   * [`seed_env`]  — a low-confidence pre-probe guess from the environment.
//!
//! The only impure part — writing the batch and draining replies from the real terminal — lives in
//! `main.rs`, which owns all IO (see `lib.rs`).

pub mod ledger;
pub mod probe;
pub mod sequences;

use ledger::{
    CapValue, Capability, Confidence, Entry, GraphicsProtocol, KeyEncoding, Ledger, Source,
};

/// Seed the ledger from the environment BEFORE probing — a hint, never the verdict (architecture
/// §6.1/§6.3: "do not identify a terminal by `TERM` alone"). Everything here is `Source::EnvHint`,
/// so a DA1-fenced probe reply overrides it and a user override overrides both. Pure in its inputs
/// (the caller reads the actual env), so it is testable without a process environment.
///
/// * `term`         — `$TERM` (e.g. `xterm-kitty` implies the kitty keyboard protocol).
/// * `colorterm`    — `$COLORTERM` (`truecolor`/`24bit`); recorded as a hint only.
/// * `term_program` — `$TERM_PROGRAM` (e.g. `WezTerm`, `kitty`).
pub fn seed_env(ledger: &mut Ledger, term: &str, _colorterm: &str, term_program: &str) {
    let hint = |value| Entry {
        value,
        source: Source::EnvHint,
        confidence: Confidence::Assumed,
    };

    let looks_kitty = term.contains("kitty") || term_program.eq_ignore_ascii_case("kitty");
    if looks_kitty {
        ledger.record(
            Capability::KeyEncoding,
            hint(CapValue::Encoding(KeyEncoding::Kitty)),
        );
    } else if term.starts_with("xterm") || term.contains("256color") {
        ledger.record(
            Capability::KeyEncoding,
            hint(CapValue::Encoding(KeyEncoding::Xterm)),
        );
    }

    // Most modern emulators (anything advertising xterm/kitty/tmux/screen) do bracketed paste; a
    // dumb/linux console does not. This is a guess — the DECRQM probe confirms or denies it.
    let modern = looks_kitty
        || term.starts_with("xterm")
        || term.starts_with("tmux")
        || term.starts_with("screen")
        || term.contains("256color");
    if modern {
        ledger.record(Capability::BracketedPaste, hint(CapValue::Bool(true)));
    }

    // Inline graphics (F-031 slice 3b / D-053): env HINTS. Kitty from `$TERM`/`$TERM_PROGRAM`, iTerm2
    // from `$TERM_PROGRAM`. Sixel is confirmed by the DA1 probe (probe.rs), which outranks these hints.
    let graphics = if looks_kitty {
        Some(GraphicsProtocol::Kitty)
    } else if term_program.eq_ignore_ascii_case("iTerm.app")
        || term_program.eq_ignore_ascii_case("iterm2")
    {
        Some(GraphicsProtocol::ITerm2)
    } else {
        None
    };
    if let Some(g) = graphics {
        ledger.record(Capability::InlineGraphics, hint(CapValue::Graphics(g)));
    }
}

/// Apply user overrides — the highest authority, applied AFTER probing so they always win
/// (acceptance #2; architecture §6.3 "always allow user override"). These are the escape hatch for
/// a terminal that misreports its capabilities: each flag, when truthy (`1`/`true`/`yes`), forces a
/// capability OFF regardless of what the probe found. Pure in its inputs so it is testable without a
/// process environment.
///
/// * `no_kitty` — `$RUSE_NO_KITTY`: force the legacy keyboard encoding (disable kitty/xterm).
/// * `no_mouse` — `$RUSE_NO_MOUSE`: force SGR mouse off.
/// * `no_paste` — `$RUSE_NO_PASTE`: force bracketed paste off.
pub fn apply_overrides(ledger: &mut Ledger, no_kitty: &str, no_mouse: &str, no_paste: &str) {
    let truthy = |v: &str| matches!(v.trim(), "1" | "true" | "yes" | "on");
    if truthy(no_kitty) {
        ledger.set_override(
            Capability::KeyEncoding,
            CapValue::Encoding(KeyEncoding::Legacy),
        );
    }
    if truthy(no_mouse) {
        ledger.set_override(Capability::SgrMouse, CapValue::Bool(false));
    }
    if truthy(no_paste) {
        ledger.set_override(Capability::BracketedPaste, CapValue::Bool(false));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seed_env_hints_inline_graphics() {
        // Kitty terminal -> Kitty graphics hint.
        let mut k = Ledger::with_defaults();
        seed_env(&mut k, "xterm-kitty", "", "");
        assert_eq!(k.graphics(), GraphicsProtocol::Kitty);
        // iTerm2 -> iTerm2 graphics hint.
        let mut i = Ledger::with_defaults();
        seed_env(&mut i, "xterm-256color", "", "iTerm.app");
        assert_eq!(i.graphics(), GraphicsProtocol::ITerm2);
        // A plain xterm advertises no inline graphics from env alone.
        let mut x = Ledger::with_defaults();
        seed_env(&mut x, "xterm-256color", "", "");
        assert_eq!(x.graphics(), GraphicsProtocol::None);
    }

    #[test]
    fn override_forces_legacy_over_a_kitty_probe() {
        let mut l = Ledger::with_defaults();
        // Pretend the probe confirmed kitty…
        l.record(
            Capability::KeyEncoding,
            Entry {
                value: CapValue::Encoding(KeyEncoding::Kitty),
                source: Source::Probed,
                confidence: Confidence::Confirmed,
            },
        );
        // …the user forces it off, and wins.
        apply_overrides(&mut l, "1", "", "");
        assert_eq!(l.key_encoding(), KeyEncoding::Legacy);
        assert_eq!(
            l.get(Capability::KeyEncoding).unwrap().source,
            Source::UserOverride
        );
    }

    #[test]
    fn empty_override_flags_change_nothing() {
        let mut l = Ledger::with_defaults();
        l.record(
            Capability::BracketedPaste,
            Entry {
                value: CapValue::Bool(true),
                source: Source::Probed,
                confidence: Confidence::Confirmed,
            },
        );
        apply_overrides(&mut l, "", "0", "no"); // none truthy
        assert!(l.enabled(Capability::BracketedPaste));
    }

    #[test]
    fn kitty_term_hints_kitty_encoding() {
        let mut l = Ledger::with_defaults();
        seed_env(&mut l, "xterm-kitty", "truecolor", "");
        assert_eq!(l.key_encoding(), KeyEncoding::Kitty);
        assert_eq!(
            l.get(Capability::KeyEncoding).unwrap().source,
            Source::EnvHint
        );
    }

    #[test]
    fn plain_xterm_hints_xterm_not_kitty() {
        let mut l = Ledger::with_defaults();
        seed_env(&mut l, "xterm-256color", "", "");
        assert_eq!(l.key_encoding(), KeyEncoding::Xterm);
        assert!(l.enabled(Capability::BracketedPaste)); // hinted on
    }

    #[test]
    fn dumb_terminal_stays_at_safe_fallback() {
        let mut l = Ledger::with_defaults();
        seed_env(&mut l, "dumb", "", "");
        assert_eq!(l.key_encoding(), KeyEncoding::Legacy);
        assert!(!l.enabled(Capability::BracketedPaste));
    }

    #[test]
    fn env_hint_never_beats_a_user_override() {
        let mut l = Ledger::with_defaults();
        l.set_override(
            Capability::KeyEncoding,
            CapValue::Encoding(KeyEncoding::Legacy),
        );
        seed_env(&mut l, "xterm-kitty", "", "kitty");
        assert_eq!(l.key_encoding(), KeyEncoding::Legacy); // override held
    }
}
