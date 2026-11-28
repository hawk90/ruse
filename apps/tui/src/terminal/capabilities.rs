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
                    // Best-effort; env/default belief stands on failure, and the log says so (#966).
                    match live_probe(ledger, &mut typed) {
                        Ok(true) => {}
                        Ok(false) => crate::log::degrade("terminal", "probe_unanswered", ""),
                        Err(_) => crate::log::degrade("terminal", "probe_failed", ""),
                    }
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
        &env("RUSE_NO_FOCUS"),
        &env("RUSE_GRAPHICS"),
    );
    ledger
}

/// The liveness net of the probe drain: how long, IN ALL, it waits for the DA1 fence before giving up on a
/// terminal that never answers DA1. It is not a per-capability or per-read timeout: a reply that takes
/// 200 ms over a slow SSH link still lands, and the drain ends the moment the fence arrives (#777).
const PROBE_DEADLINE: std::time::Duration = std::time::Duration::from_secs(1);

/// Emit the probe batch and drain the terminal's replies until the DA1 fence (F-010 acceptance #1).
/// [`PROBE_DEADLINE`] is a LIVENESS net for a terminal that never answers DA1 — NOT a per-capability
/// timeout; the fence, not the clock, decides support (see `caps::probe`). What the user typed meanwhile
/// goes to `typed`, even when a read fails. `Ok(false)` when the fence never replied: the capabilities
/// the terminal did not confirm keep their env/default belief.
#[cfg(unix)]
fn live_probe(ledger: &mut caps::ledger::Ledger, typed: &mut Vec<u8>) -> io::Result<bool> {
    let mut parser = caps::probe::ProbeParser::new();
    let res = send_and_drain(ledger, &mut parser);
    let fenced = parser.is_fenced();
    *typed = parser.take_typed();
    res.map(|()| fenced)
}

#[cfg(unix)]
fn send_and_drain(
    ledger: &mut caps::ledger::Ledger,
    parser: &mut caps::probe::ProbeParser,
) -> io::Result<()> {
    let mut out = io::stdout();
    out.write_all(&caps::probe::query_batch())?;
    out.flush()?;
    drain_until_fence(
        &mut StdinSource,
        std::time::Instant::now,
        PROBE_DEADLINE,
        ledger,
        parser,
    )
}

/// The probe's input: one read that waits at most `timeout`. `Ok(None)` when nothing arrived in time (or
/// the wait was interrupted); `Ok(Some(0))` at end of input.
#[cfg(any(unix, test))]
trait ProbeSource {
    fn read_within(
        &mut self,
        timeout: std::time::Duration,
        buf: &mut [u8],
    ) -> io::Result<Option<usize>>;
}

/// The live tty: `poll(2)` on stdin, then one `read`.
#[cfg(unix)]
struct StdinSource;

#[cfg(unix)]
impl ProbeSource for StdinSource {
    fn read_within(
        &mut self,
        timeout: std::time::Duration,
        buf: &mut [u8],
    ) -> io::Result<Option<usize>> {
        use std::io::Read;
        use std::os::unix::io::AsRawFd;

        // Rounded up, so a sub-millisecond remainder still waits instead of spinning on a zero timeout.
        let ms = i32::try_from(timeout.as_micros().div_ceil(1000)).unwrap_or(i32::MAX);
        let mut pfd = libc::pollfd {
            fd: io::stdin().as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: `pfd` is a valid, initialised `pollfd`; `poll` reads/writes only that one struct.
        let ready = unsafe { libc::poll(&mut pfd, 1, ms) };
        if ready < 0 {
            let err = io::Error::last_os_error();
            return if err.kind() == io::ErrorKind::Interrupted {
                Ok(None) // a signal (SIGWINCH): the caller re-waits for what is left of the deadline
            } else {
                Err(err)
            };
        }
        if ready == 0 {
            return Ok(None);
        }
        io::stdin().read(buf).map(Some)
    }
}

/// Feed `src` into `parser` until the DA1 fence replies, the input ends, or `deadline` (measured on `now`)
/// has passed since the drain began. Silence alone never ends the drain early: each wait is for whatever
/// is left of the deadline, so a slow link's reply still counts (#777). Nothing after the fence is read.
#[cfg(any(unix, test))]
fn drain_until_fence(
    src: &mut impl ProbeSource,
    now: impl Fn() -> std::time::Instant,
    deadline: std::time::Duration,
    ledger: &mut caps::ledger::Ledger,
    parser: &mut caps::probe::ProbeParser,
) -> io::Result<()> {
    let start = now();
    let mut buf = [0u8; 512];
    while !parser.is_fenced() {
        let left = deadline.saturating_sub(now().saturating_duration_since(start));
        if left.is_zero() {
            break; // liveness net: whatever replied so far stands, defaults for the rest
        }
        match src.read_within(left, &mut buf)? {
            None => {}        // nothing yet: wait out the rest of the deadline
            Some(0) => break, // end of input
            Some(n) => {
                parser.feed(&buf[..n], ledger);
            }
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

    /// What an xterm-like TERM pushes on its env hints alone: bracketed paste, then focus reports.
    const PASTE_FOCUS: &[u8] = b"\x1b[?2004h\x1b[?1004h";

    #[test]
    fn term_matrix_without_a_probe_reply() {
        // (TERM, COLORTERM, key encoding, bracketed paste, bytes pushed on enter). Focus reporting follows
        // the bracketed-paste hint, plus rxvt (#1220).
        let rows: &[(&str, &str, KeyEncoding, bool, &[u8])] = &[
            ("", "", KeyEncoding::Legacy, false, b""),
            ("dumb", "", KeyEncoding::Legacy, false, b""),
            ("linux", "", KeyEncoding::Legacy, false, b""),
            ("ansi", "", KeyEncoding::Legacy, false, b""),
            ("vt100", "", KeyEncoding::Legacy, false, b""),
            ("xterm", "", KeyEncoding::Legacy, true, PASTE_FOCUS),
            ("xterm-256color", "", KeyEncoding::Legacy, true, PASTE_FOCUS),
            ("screen", "", KeyEncoding::Legacy, true, PASTE_FOCUS),
            (
                "screen-256color",
                "",
                KeyEncoding::Legacy,
                true,
                PASTE_FOCUS,
            ),
            ("tmux", "", KeyEncoding::Legacy, true, PASTE_FOCUS),
            ("tmux-256color", "", KeyEncoding::Legacy, true, PASTE_FOCUS),
            (
                "rxvt-unicode",
                "",
                KeyEncoding::Legacy,
                false,
                b"\x1b[?1004h",
            ),
            (
                "rxvt-unicode-256color",
                "",
                KeyEncoding::Legacy,
                true,
                PASTE_FOCUS,
            ),
            (
                "xterm-kitty",
                "",
                KeyEncoding::Kitty,
                true,
                b"\x1b[>3u\x1b[?2004h\x1b[?1004h",
            ),
            // COLORTERM is read but carries no capability yet: colour depth is not modelled (#627), so
            // truecolor changes nothing the ledger decides.
            (
                "xterm-256color",
                "truecolor",
                KeyEncoding::Legacy,
                true,
                PASTE_FOCUS,
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
        b"[?1004;2$y\x1b[?62;22c",
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
                Capability::FocusEvents,
            ] {
                assert!(l.enabled(cap), "TERM={term:?}: {cap:?}");
                assert_eq!(l.get(cap).map(|e| e.source), Some(Source::Probed));
            }
        }
    }

    #[test]
    fn a_fenced_denial_overrides_the_env_hint() {
        // xterm-256color hints bracketed paste and focus reports; the terminal answers "mode not
        // recognised" for both before DA1.
        let (l, fenced) = with_reply(
            &[("TERM", "xterm-256color")],
            &[b"\x1b[?2004;0$y\x1b[?1004;0$y\x1b[?1;2c"],
        );
        assert!(fenced);
        assert!(!l.enabled(Capability::BracketedPaste));
        assert!(!l.enabled(Capability::FocusEvents));
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
            ("RUSE_NO_FOCUS", "1"),
        ];
        let (l, _) = with_reply(&vars, MODERN);
        assert_eq!(l.key_encoding(), KeyEncoding::Legacy);
        assert!(!l.enabled(Capability::SgrMouse));
        assert!(!l.enabled(Capability::BracketedPaste));
        assert!(!l.enabled(Capability::FocusEvents));
        assert_eq!(
            l.get(Capability::KeyEncoding).map(|e| e.source),
            Some(Source::UserOverride)
        );
        // The rest of the probe result stands.
        assert!(l.enabled(Capability::SynchronizedOutput));
    }

    /// A scripted terminal on a fake clock (#777): each reply arrives at its time since the drain began.
    /// A wait that sees no reply in time advances the clock by the whole wait.
    struct Scripted {
        replies: std::collections::VecDeque<(u64, &'static [u8])>,
        clock: std::rc::Rc<std::cell::Cell<std::time::Duration>>,
        reads: usize,
    }

    impl Scripted {
        fn new(replies: &[(u64, &'static [u8])]) -> Scripted {
            Scripted {
                replies: replies.iter().copied().collect(),
                clock: Default::default(),
                reads: 0,
            }
        }

        fn ms(&self) -> u128 {
            self.clock.get().as_millis()
        }
    }

    impl ProbeSource for Scripted {
        fn read_within(
            &mut self,
            timeout: std::time::Duration,
            buf: &mut [u8],
        ) -> io::Result<Option<usize>> {
            let now = self.clock.get();
            match self.replies.front() {
                Some(&(at, bytes)) if std::time::Duration::from_millis(at) <= now + timeout => {
                    self.replies.pop_front();
                    self.clock
                        .set(now.max(std::time::Duration::from_millis(at)));
                    self.reads += 1;
                    buf[..bytes.len()].copy_from_slice(bytes);
                    Ok(Some(bytes.len()))
                }
                Some(_) => {
                    self.clock.set(now + timeout);
                    Ok(None)
                }
                // The script is over: the input ends.
                None => Ok(Some(0)),
            }
        }
    }

    /// Run the real drain over `src`, from the default ledger.
    fn drain(src: &mut Scripted) -> (Ledger, bool) {
        let mut ledger = Ledger::with_defaults();
        let mut parser = ProbeParser::new();
        let base = std::time::Instant::now();
        let clock = src.clock.clone();
        drain_until_fence(
            src,
            move || base + clock.get(),
            PROBE_DEADLINE,
            &mut ledger,
            &mut parser,
        )
        .unwrap();
        (ledger, parser.is_fenced())
    }

    /// A terminal that never answers again: a reply scheduled far past any deadline.
    const NEVER: (u64, &[u8]) = (1 << 40, b"");

    #[test]
    fn a_reply_over_a_slow_link_still_counts() {
        // A 200 ms round trip (#777): the old drain gave up after the first 50 ms of silence.
        let mut src = Scripted::new(&[(200, b"\x1b[?1006;2$y\x1b[?0u\x1b[?62c"), NEVER]);
        let (l, fenced) = drain(&mut src);
        assert!(fenced);
        assert!(l.enabled(Capability::SgrMouse));
        assert_eq!(l.key_encoding(), KeyEncoding::Kitty);
        // The fence ends the drain at once: nothing after it is waited for.
        assert_eq!(src.ms(), 200);
    }

    #[test]
    fn silence_between_reply_chunks_does_not_end_the_drain() {
        let mut src = Scripted::new(&[
            (10, b"\x1b[?2026;1$y"),
            (400, b"\x1b[?1006;2$y"),
            (700, b"\x1b[?62c"),
            NEVER,
        ]);
        let (l, fenced) = drain(&mut src);
        assert!(fenced);
        assert!(l.enabled(Capability::SynchronizedOutput));
        assert!(l.enabled(Capability::SgrMouse));
        assert_eq!(src.reads, 3);
    }

    #[test]
    fn a_terminal_that_never_fences_ends_at_the_deadline() {
        let mut src = Scripted::new(&[(5, b"\x1b[?2026;1$y"), NEVER]);
        let (l, fenced) = drain(&mut src);
        assert!(!fenced);
        // What replied before the deadline stands.
        assert!(l.enabled(Capability::SynchronizedOutput));
        assert_eq!(src.ms(), PROBE_DEADLINE.as_millis());
    }

    #[test]
    fn a_reply_past_the_deadline_is_not_read() {
        let late = PROBE_DEADLINE.as_millis() as u64 + 1;
        let mut src = Scripted::new(&[(late, b"\x1b[?62c")]);
        let (_, fenced) = drain(&mut src);
        assert!(!fenced);
        assert_eq!(src.reads, 0);
    }

    #[test]
    fn the_end_of_input_ends_the_drain() {
        let mut src = Scripted::new(&[(1, b"\x1b[?2026;1$y")]);
        let (l, fenced) = drain(&mut src);
        assert!(!fenced);
        assert!(l.enabled(Capability::SynchronizedOutput));
        assert_eq!(src.ms(), 1);
    }

    #[test]
    fn term_program_hints_without_term() {
        let l = no_reply(&[("TERM_PROGRAM", "kitty")]);
        assert_eq!(l.key_encoding(), KeyEncoding::Kitty);
    }
}
