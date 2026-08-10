//! Integration proof of the DA1-fenced probe protocol (F-010 acceptance #1), driven by canned
//! terminal reply streams instead of a live tty — the same "test the pure engine without a
//! terminal" discipline the input engine uses (see `apps/tui/src/lib.rs`).
//!
//! What is proven here that the inline unit tests do not: whole-stream folding, support decided by
//! reply-ORDER-before-the-fence (a capability that never replies is UNSUPPORTED, not unknown), and
//! correct handling of a CSI reply split across two `feed` calls.

use ruse_tui::caps::ledger::{Capability, KeyEncoding, Ledger, Source};
use ruse_tui::caps::probe::{query_batch, ProbeParser, ProbeState};

/// A DECRQM report `CSI ? <mode> ; <state> $ y`.
fn decrqm(mode: u16, state: u8) -> Vec<u8> {
    format!("\x1b[?{mode};{state}$y").into_bytes()
}

const KITTY_REPLY: &[u8] = b"\x1b[?1u"; // CSI ? <flags> u
const DA1: &[u8] = b"\x1b[?62;22c"; // a typical DA1 reply — the fence

fn drive(chunks: &[&[u8]]) -> (Ledger, ProbeParser) {
    let mut ledger = Ledger::with_defaults();
    let mut parser = ProbeParser::new();
    for c in chunks {
        parser.feed(c, &mut ledger);
    }
    (ledger, parser)
}

#[test]
fn batch_is_da1_fenced_last() {
    let batch = query_batch();
    // The DA1 request must be the final bytes so the reply order is a valid fence.
    assert!(
        batch.ends_with(b"\x1b[c"),
        "probe batch must end with the DA1 fence"
    );
    // And it must actually probe (kitty query present).
    assert!(
        batch.windows(4).any(|w| w == b"\x1b[?u"),
        "batch must include the kitty query"
    );
}

#[test]
fn modern_terminal_confirms_everything() {
    let mut stream = Vec::new();
    stream.extend(decrqm(2004, 1)); // bracketed paste: set
    stream.extend(decrqm(1006, 1)); // SGR mouse: set
    stream.extend(decrqm(2026, 2)); // synchronized output: reset — but RECOGNISED = supported
    stream.extend(decrqm(2048, 1)); // resize events: set
    stream.extend_from_slice(KITTY_REPLY);
    stream.extend_from_slice(DA1);

    let (ledger, parser) = drive(&[&stream]);
    assert!(parser.is_fenced());
    assert!(ledger.enabled(Capability::BracketedPaste));
    assert!(ledger.enabled(Capability::SgrMouse));
    assert!(ledger.enabled(Capability::SynchronizedOutput)); // recognised, even though currently off
    assert!(ledger.enabled(Capability::ResizeEvents));
    assert_eq!(ledger.key_encoding(), KeyEncoding::Kitty);
    assert_eq!(
        ledger.get(Capability::SgrMouse).unwrap().source,
        Source::Probed
    );
}

#[test]
fn bare_terminal_answers_only_da1() {
    // No DECRQM, no kitty — just the fence. Everything must stay at the safe fallback: reply order
    // proved they are unsupported, NOT that they are unknown.
    let (ledger, parser) = drive(&[DA1]);
    assert!(parser.is_fenced());
    assert!(!ledger.enabled(Capability::BracketedPaste));
    assert!(!ledger.enabled(Capability::SgrMouse));
    assert_eq!(ledger.key_encoding(), KeyEncoding::Legacy);
    assert_eq!(
        ledger.get(Capability::BracketedPaste).unwrap().source,
        Source::Default
    );
}

#[test]
fn decrqm_not_recognised_is_unsupported() {
    let mut stream = decrqm(2004, 0); // state 0 = not recognised
    stream.extend_from_slice(DA1);
    let (ledger, _) = drive(&[&stream]);
    assert!(!ledger.enabled(Capability::BracketedPaste));
}

#[test]
fn reply_split_across_reads_is_reassembled() {
    let mut full = decrqm(2004, 1);
    full.extend_from_slice(KITTY_REPLY);
    full.extend_from_slice(DA1);
    // Split in the MIDDLE of the first CSI sequence (after "\x1b[?20").
    let cut = 5;
    let (ledger, parser) = drive(&[&full[..cut], &full[cut..]]);
    assert!(parser.is_fenced());
    assert!(ledger.enabled(Capability::BracketedPaste));
    assert_eq!(ledger.key_encoding(), KeyEncoding::Kitty);
}

#[test]
fn stray_input_before_fence_is_ignored() {
    // A keystroke ('j') arriving amid the replies is not a known CSI reply and must be skipped
    // without derailing the parse or the fence.
    let mut stream = decrqm(2004, 1);
    stream.push(b'j');
    stream.extend_from_slice(DA1);
    let (ledger, parser) = drive(&[&stream]);
    assert!(parser.is_fenced());
    assert!(ledger.enabled(Capability::BracketedPaste));
}

#[test]
fn awaiting_until_the_fence_arrives() {
    let mut ledger = Ledger::with_defaults();
    let mut parser = ProbeParser::new();
    assert_eq!(
        parser.feed(&decrqm(2004, 1), &mut ledger),
        ProbeState::Awaiting
    );
    assert_eq!(parser.feed(DA1, &mut ledger), ProbeState::Fenced);
}
