//! F-010 capability detection: build the capability ledger from safe defaults, an env seed, and — on
//! a real Unix tty — a DA1-fenced active probe, then apply user overrides.

use std::io::{self, Write};

use crate::caps;

/// Build the capability ledger (F-010): safe-fallback defaults, then a low-confidence env seed,
/// then — on a real Unix tty — a DA1-fenced active probe that upgrades what the terminal confirms.
/// Every step is non-fatal: a probe that fails or is skipped leaves the honest env/default belief.
/// Also returns the bytes the user typed while the probe read the terminal (#1013), for the event loop.
pub(crate) fn detect_capabilities() -> (caps::ledger::Ledger, Vec<u8>) {
    let mut typed = Vec::new();
    let ledger = build_ledger(
        |name| std::env::var(name).unwrap_or_default(),
        |ledger| {
            #[cfg(unix)]
            {
                use std::io::IsTerminal;
                if io::stdin().is_terminal() {
                    // Best-effort; env/default belief stands on failure.
                    let _ = live_probe(ledger, &mut typed);
                }
            }
            #[cfg(not(unix))]
            let _ = ledger;
        },
    );
    (ledger, typed)
}

/// The detection order, pure in its inputs so the TERM/COLORTERM matrix unit-tests without a process
/// environment or a tty (#896): `env` reads a variable (empty when unset), and `probe` folds the
/// terminal's replies into the ledger between the env seed and the user overrides.
fn build_ledger(
    env: impl Fn(&str) -> String,
    probe: impl FnOnce(&mut caps::ledger::Ledger),
) -> caps::ledger::Ledger {
    let mut ledger = caps::ledger::Ledger::with_defaults();
    caps::seed_env(
        &mut ledger,
        &env("TERM"),
        &env("COLORTERM"),
        &env("TERM_PROGRAM"),
    );
    probe(&mut ledger);
    // User overrides win over everything the probe found (architecture §6.3).
    caps::apply_overrides(
        &mut ledger,
        &env("RUSE_NO_KITTY"),
        &env("RUSE_NO_MOUSE"),
        &env("RUSE_NO_PASTE"),
        &env("RUSE_GRAPHICS"),
    );
    ledger
}

/// Emit the probe batch and drain the terminal's replies until the DA1 fence (F-010 acceptance #1).
/// The `poll` deadline is a LIVENESS net for a terminal that never answers DA1 — NOT a
/// per-capability timeout; the fence, not the clock, decides support (see `caps::probe`). What the user
/// typed meanwhile goes to `typed`, even when a read fails.
#[cfg(unix)]
fn live_probe(ledger: &mut caps::ledger::Ledger, typed: &mut Vec<u8>) -> io::Result<()> {
    let mut parser = caps::probe::ProbeParser::new();
    let res = drain_probe(ledger, &mut parser);
    *typed = parser.take_typed();
    res
}

#[cfg(unix)]
fn drain_probe(
    ledger: &mut caps::ledger::Ledger,
    parser: &mut caps::probe::ProbeParser,
) -> io::Result<()> {
    use std::io::Read;
    use std::os::unix::io::AsRawFd;

    let mut out = io::stdout();
    out.write_all(&caps::probe::query_batch())?;
    out.flush()?;

    let fd = io::stdin().as_raw_fd();
    let mut buf = [0u8; 512];
    // Up to ~20 × 50 ms only if the terminal keeps sending nothing; a real terminal answers in the
    // first poll and the fence breaks the loop immediately.
    for _ in 0..20 {
        // SAFETY: `pfd` is a valid, initialised `pollfd`; `poll` reads/writes only that one struct.
        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let ready = unsafe { libc::poll(&mut pfd, 1, 50) };
        if ready <= 0 {
            break; // timeout or error — stop; whatever replied so far stands, defaults for the rest
        }
        let n = io::stdin().read(&mut buf)?;
        if n == 0 {
            break;
        }
        parser.feed(&buf[..n], ledger);
        if parser.is_fenced() {
            break; // the DA1 fence replied — the ledger is final
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    //! The TERM / COLORTERM matrix (#896): env plus canned probe replies through the real detection order.
    //! `build_ledger` takes the environment as a function, so nothing here reads the process environment
    //! (a developer's `TMUX` or `COLORTERM` cannot leak in).

    use super::*;
    use crate::caps::ledger::{Capability, KeyEncoding, Ledger, Source};
    use crate::caps::probe::ProbeParser;

    fn env_of<'a>(vars: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> String + 'a {
        move |name| {
            vars.iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| (*v).to_owned())
                .unwrap_or_default()
        }
    }

    /// Detection with no tty reply: the env seed and the overrides only.
    fn no_reply(vars: &[(&str, &str)]) -> Ledger {
        build_ledger(env_of(vars), |_| {})
    }

    /// Detection whose probe receives `chunks`, one read each. Returns whether the DA1 fence arrived.
    fn with_reply(vars: &[(&str, &str)], chunks: &[&[u8]]) -> (Ledger, bool) {
        let mut fenced = false;
        let ledger = build_ledger(env_of(vars), |l| {
            let mut parser = ProbeParser::new();
            for c in chunks {
                parser.feed(c, l);
            }
            fenced = parser.is_fenced();
        });
        (ledger, fenced)
    }

    const PASTE_ONLY: &[u8] = b"\x1b[?2004h";

    #[test]
    fn term_matrix_without_a_probe_reply() {
        // (TERM, COLORTERM, key encoding, bracketed paste, bytes pushed on enter)
        let rows: &[(&str, &str, KeyEncoding, bool, &[u8])] = &[
            ("", "", KeyEncoding::Legacy, false, b""),
            ("dumb", "", KeyEncoding::Legacy, false, b""),
            ("linux", "", KeyEncoding::Legacy, false, b""),
            ("ansi", "", KeyEncoding::Legacy, false, b""),
            ("vt100", "", KeyEncoding::Legacy, false, b""),
            ("xterm", "", KeyEncoding::Legacy, true, PASTE_ONLY),
            ("xterm-256color", "", KeyEncoding::Legacy, true, PASTE_ONLY),
            ("screen", "", KeyEncoding::Legacy, true, PASTE_ONLY),
            ("screen-256color", "", KeyEncoding::Legacy, true, PASTE_ONLY),
            ("tmux", "", KeyEncoding::Legacy, true, PASTE_ONLY),
            ("tmux-256color", "", KeyEncoding::Legacy, true, PASTE_ONLY),
            ("rxvt-unicode", "", KeyEncoding::Legacy, false, b""),
            (
                "rxvt-unicode-256color",
                "",
                KeyEncoding::Legacy,
                true,
                PASTE_ONLY,
            ),
            (
                "xterm-kitty",
                "",
                KeyEncoding::Kitty,
                true,
                b"\x1b[>3u\x1b[?2004h",
            ),
            // COLORTERM is read but carries no capability yet: colour depth is not modelled (#627), so
            // truecolor changes nothing the ledger decides.
            (
                "xterm-256color",
                "truecolor",
                KeyEncoding::Legacy,
                true,
                PASTE_ONLY,
            ),
            ("dumb", "truecolor", KeyEncoding::Legacy, false, b""),
            ("linux", "24bit", KeyEncoding::Legacy, false, b""),
        ];
        for &(term, colorterm, enc, paste, enter) in rows {
            let l = no_reply(&[("TERM", term), ("COLORTERM", colorterm)]);
            let id = format!("TERM={term:?} COLORTERM={colorterm:?}");
            assert_eq!(l.key_encoding(), enc, "{id}");
            assert_eq!(l.enabled(Capability::BracketedPaste), paste, "{id}");
            // Without a reply nothing is confirmed: mouse, sync output and resize stay off.
            for cap in [
                Capability::SgrMouse,
                Capability::SynchronizedOutput,
                Capability::ResizeEvents,
            ] {
                assert!(!l.enabled(cap), "{id}: {cap:?}");
            }
            assert_eq!(caps::sequences::enter(&l), enter, "{id}");
            // Whatever was pushed is popped on exit.
            assert_eq!(
                caps::sequences::exit(&l).is_empty(),
                enter.is_empty(),
                "{id}"
            );
        }
    }

    /// A modern terminal's full reply, split mid-sequence across reads.
    const MODERN: &[&[u8]] = &[
        b"\x1b[?2004;2$y\x1b[?1006;2$y\x1b[?20",
        b"26;2$y\x1b[?2048;2$y\x1b[?0u\x1b",
        b"[?62;22c",
    ];

    #[test]
    fn a_probe_reply_upgrades_any_term() {
        for term in ["", "dumb", "linux", "xterm-256color", "tmux-256color"] {
            let (l, fenced) = with_reply(&[("TERM", term)], MODERN);
            assert!(fenced, "TERM={term:?}");
            assert_eq!(l.key_encoding(), KeyEncoding::Kitty, "TERM={term:?}");
            for cap in [
                Capability::BracketedPaste,
                Capability::SgrMouse,
                Capability::SynchronizedOutput,
                Capability::ResizeEvents,
            ] {
                assert!(l.enabled(cap), "TERM={term:?}: {cap:?}");
                assert_eq!(l.get(cap).map(|e| e.source), Some(Source::Probed));
            }
        }
    }

    #[test]
    fn a_fenced_denial_overrides_the_env_hint() {
        // xterm-256color hints bracketed paste; the terminal answers "mode not recognised" before DA1.
        let (l, fenced) = with_reply(
            &[("TERM", "xterm-256color")],
            &[b"\x1b[?2004;0$y\x1b[?1;2c"],
        );
        assert!(fenced);
        assert!(!l.enabled(Capability::BracketedPaste));
        // Nothing left to push: modifyOtherKeys is never requested (#772).
        assert!(caps::sequences::enter(&l).is_empty());
    }

    #[test]
    fn silence_before_the_fence_keeps_the_env_belief() {
        // DA1 alone: no mode answered. The ledger does not downgrade an env hint it got no denial for, so
        // TERM=xterm-kitty keeps the kitty push even when the terminal never answered `CSI ? u`.
        let (l, fenced) = with_reply(&[("TERM", "xterm-kitty")], &[b"\x1b[?62c"]);
        assert!(fenced);
        assert_eq!(l.key_encoding(), KeyEncoding::Kitty);
        assert!(l.enabled(Capability::BracketedPaste));
        // A dumb terminal that answers DA1 only stays at the safe fallback.
        let (l, _) = with_reply(&[("TERM", "dumb")], &[b"\x1b[?1;0c"]);
        assert!(caps::sequences::enter(&l).is_empty());
    }

    #[test]
    fn an_unfenced_partial_reply_still_counts() {
        // The terminal answered one mode and then went quiet (the poll deadline ends the drain).
        let (l, fenced) = with_reply(&[("TERM", "dumb")], &[b"\x1b[?2026;1$y\x1b[?20"]);
        assert!(!fenced);
        assert!(l.enabled(Capability::SynchronizedOutput));
        assert!(!l.enabled(Capability::BracketedPaste));
    }

    #[test]
    fn overrides_win_over_env_and_probe() {
        let vars = [
            ("TERM", "xterm-kitty"),
            ("RUSE_NO_KITTY", "1"),
            ("RUSE_NO_MOUSE", "yes"),
            ("RUSE_NO_PASTE", "true"),
        ];
        let (l, _) = with_reply(&vars, MODERN);
        assert_eq!(l.key_encoding(), KeyEncoding::Legacy);
        assert!(!l.enabled(Capability::SgrMouse));
        assert!(!l.enabled(Capability::BracketedPaste));
        assert_eq!(
            l.get(Capability::KeyEncoding).map(|e| e.source),
            Some(Source::UserOverride)
        );
        // The rest of the probe result stands.
        assert!(l.enabled(Capability::SynchronizedOutput));
    }

    #[test]
    fn term_program_hints_without_term() {
        let l = no_reply(&[("TERM_PROGRAM", "kitty")]);
        assert_eq!(l.key_encoding(), KeyEncoding::Kitty);
    }
}
