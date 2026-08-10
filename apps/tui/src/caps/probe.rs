//! Active, DA1-fenced capability probing (F-010 acceptance #1).
//!
//! The mechanism is neovim's (`src/nvim/tui/tui.c`), censused as the `term_mode` + `key_encoding`
//! surfaces: emit every capability query, then a **Primary Device Attributes** request (`CSI c`,
//! universally answered) as an ordering FENCE. The terminal answers queries in order, so once the
//! DA1 reply arrives every capability it supports has already replied — support is proven by reply
//! ORDER, with **no arbitrary per-capability timeout**. A query that drew no reply before the fence
//! is unsupported, not "unknown".
//!
//! This module is pure: [`query_batch`] builds the outbound bytes, [`ProbeParser`] folds an inbound
//! byte stream into a [`Ledger`] and reports when the fence is reached. The live drain that pushes
//! the bytes and reads the replies lives in `main.rs`; the protocol logic is tested here against
//! canned streams (see `tests/caps_probe.rs`).

use super::ledger::{CapValue, Capability, Confidence, Entry, KeyEncoding, Ledger, Source};

/// DEC private mode numbers we query via DECRQM (`CSI ? <n> $ p`), paired with the capability each
/// proves. Mouse is three cooperating modes; SGR-1006 is the one whose support gates the feature.
const DECRQM_MODES: &[(u16, Capability)] = &[
    (2004, Capability::BracketedPaste),
    (1006, Capability::SgrMouse),
    (2026, Capability::SynchronizedOutput),
    (2048, Capability::ResizeEvents),
];

/// The outbound probe: a DECRQM query per mode, the kitty keyboard query (`CSI ? u`), then the DA1
/// fence (`CSI c`). Exactly the `\x1b[?u\x1b[c` pairing neovim's `tui_query_kitty_keyboard` emits,
/// generalised over every mode. The trailing DA1 is what makes the read bounded without a timeout.
pub fn query_batch() -> Vec<u8> {
    let mut out = Vec::new();
    for (mode, _) in DECRQM_MODES {
        out.extend_from_slice(format!("\x1b[?{mode}$p").as_bytes());
    }
    out.extend_from_slice(b"\x1b[?u"); // kitty progressive-enhancement query
    out.extend_from_slice(b"\x1b[c"); // DA1 fence — MUST be last
    out
}

/// Where the probe stands after the bytes fed so far.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ProbeState {
    /// The DA1 reply has not arrived yet — keep reading.
    Awaiting,
    /// The DA1 fence replied; probing is complete and the ledger is final.
    Fenced,
}

/// Incrementally folds terminal replies into a [`Ledger`]. Handles CSI sequences split across reads.
#[derive(Default)]
pub struct ProbeParser {
    /// Bytes of an in-progress escape sequence, or leftover partial input between `feed` calls.
    pending: Vec<u8>,
    fenced: bool,
}

impl ProbeParser {
    pub fn new() -> Self {
        ProbeParser::default()
    }

    /// True once the DA1 fence has been seen.
    pub fn is_fenced(&self) -> bool {
        self.fenced
    }

    /// Fold a chunk of terminal output into `ledger`. Recognises three CSI replies and ignores
    /// everything else (a well-behaved terminal sends nothing else before the fence; stray user
    /// input is simply not a known reply and is skipped):
    ///   * DECRQM report `CSI ? <mode> ; <state> $ y` — state != 0 means the mode is recognised.
    ///   * kitty report  `CSI ? <flags> u`            — the kitty keyboard protocol is present.
    ///   * DA1 report    `CSI ? <params> c`           — the fence; probing is done.
    pub fn feed(&mut self, bytes: &[u8], ledger: &mut Ledger) -> ProbeState {
        self.pending.extend_from_slice(bytes);
        let mut i = 0;
        while i < self.pending.len() {
            // Seek the next CSI introducer (ESC '[').
            if self.pending[i] != 0x1b {
                i += 1;
                continue;
            }
            match self.pending.get(i + 1) {
                None => break, // ESC at the tail — wait for more bytes.
                Some(&b'[') => {}
                Some(_) => {
                    i += 1;
                    continue;
                }
            }
            // Collect the parameter/intermediate bytes up to the FINAL byte (0x40..=0x7e).
            let mut j = i + 2;
            while j < self.pending.len() && !(0x40..=0x7e).contains(&self.pending[j]) {
                j += 1;
            }
            if j == self.pending.len() {
                break; // Incomplete sequence — keep it in `pending` for the next feed.
            }
            let seq = &self.pending[i + 2..=j]; // params.. + final byte
            if Self::classify(seq, ledger) {
                self.fenced = true;
            }
            i = j + 1;
        }
        // Drop everything we have consumed or skipped; keep only a trailing partial sequence.
        self.pending.drain(..i);
        if self.fenced {
            ProbeState::Fenced
        } else {
            ProbeState::Awaiting
        }
    }

    /// Classify one complete CSI body (`<params><final>`), update the ledger, and return whether
    /// this reply was the DA1 fence. Static (no `&mut self`) so the caller can hold an immutable
    /// slice of `pending` while calling it.
    fn classify(seq: &[u8], ledger: &mut Ledger) -> bool {
        let (&final_byte, params) = match seq.split_last() {
            Some(x) => x,
            None => return false,
        };
        match final_byte {
            b'c' if params.first() == Some(&b'?') => {
                // DA1 reply (`CSI ? ... c`). The fence — nothing more will come.
                return true;
            }
            b'u' if params.first() == Some(&b'?') => {
                // Kitty keyboard reply (`CSI ? <flags> u`). Its mere arrival proves support.
                ledger.record(
                    Capability::KeyEncoding,
                    Entry {
                        value: CapValue::Encoding(KeyEncoding::Kitty),
                        source: Source::Probed,
                        confidence: Confidence::Confirmed,
                    },
                );
            }
            b'y' => {
                // DECRQM report `CSI ? <mode> ; <state> $ y`. `$` is the byte before `y`.
                if params.last() != Some(&b'$') || params.first() != Some(&b'?') {
                    return false;
                }
                let inner = &params[1..params.len() - 1]; // between '?' and '$'
                let mut halves = inner.split(|&b| b == b';');
                let mode = halves.next().and_then(parse_u16);
                let state = halves.next().and_then(parse_u16);
                if let (Some(mode), Some(state)) = (mode, state) {
                    if let Some((_, cap)) = DECRQM_MODES.iter().find(|(m, _)| *m == mode) {
                        // state 0 = not recognised (unsupported); 1..=4 = recognised (supported,
                        // whatever its current on/off setting).
                        let supported = state != 0;
                        ledger.record(
                            *cap,
                            Entry {
                                value: CapValue::Bool(supported),
                                source: Source::Probed,
                                confidence: Confidence::Confirmed,
                            },
                        );
                    }
                }
            }
            _ => {}
        }
        false
    }
}

fn parse_u16(bytes: &[u8]) -> Option<u16> {
    if bytes.is_empty() {
        return None;
    }
    let mut n: u16 = 0;
    for &b in bytes {
        let d = b.checked_sub(b'0').filter(|d| *d <= 9)?;
        n = n.checked_mul(10)?.checked_add(u16::from(d))?;
    }
    Some(n)
}
