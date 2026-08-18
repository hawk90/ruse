//! F-010 capability detection: build the capability ledger from safe defaults, an env seed, and — on
//! a real Unix tty — a DA1-fenced active probe, then apply user overrides.

use std::io::{self, Write};

use crate::caps;

/// Build the capability ledger (F-010): safe-fallback defaults, then a low-confidence env seed,
/// then — on a real Unix tty — a DA1-fenced active probe that upgrades what the terminal confirms.
/// Every step is non-fatal: a probe that fails or is skipped leaves the honest env/default belief.
pub(crate) fn detect_capabilities() -> caps::ledger::Ledger {
    let mut ledger = caps::ledger::Ledger::with_defaults();
    caps::seed_env(
        &mut ledger,
        &std::env::var("TERM").unwrap_or_default(),
        &std::env::var("COLORTERM").unwrap_or_default(),
        &std::env::var("TERM_PROGRAM").unwrap_or_default(),
    );
    #[cfg(unix)]
    {
        use std::io::IsTerminal;
        if io::stdin().is_terminal() {
            let _ = live_probe(&mut ledger); // best-effort; env/default belief stands on failure
        }
    }
    // User overrides win over everything the probe found (architecture §6.3).
    caps::apply_overrides(
        &mut ledger,
        &std::env::var("RUSE_NO_KITTY").unwrap_or_default(),
        &std::env::var("RUSE_NO_MOUSE").unwrap_or_default(),
        &std::env::var("RUSE_NO_PASTE").unwrap_or_default(),
    );
    ledger
}

/// Emit the probe batch and drain the terminal's replies until the DA1 fence (F-010 acceptance #1).
/// The `poll` deadline is a LIVENESS net for a terminal that never answers DA1 — NOT a
/// per-capability timeout; the fence, not the clock, decides support (see `caps::probe`).
#[cfg(unix)]
fn live_probe(ledger: &mut caps::ledger::Ledger) -> io::Result<()> {
    use std::io::Read;
    use std::os::unix::io::AsRawFd;

    let mut out = io::stdout();
    out.write_all(&caps::probe::query_batch())?;
    out.flush()?;

    let fd = io::stdin().as_raw_fd();
    let mut parser = caps::probe::ProbeParser::new();
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
