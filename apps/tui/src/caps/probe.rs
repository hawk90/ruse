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

use super::ledger::{
    CapValue, Capability, Confidence, Entry, GraphicsProtocol, KeyEncoding, Ledger, Source,
};

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

/// What one complete CSI sequence read during the probe was.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Reply {
    /// The DA1 reply: the fence.
    Fence,
    /// Another reply to a query of the batch.
    Answer,
    /// No reply of ours: a key the user typed (an arrow key is `CSI A`).
    NotOurs,
}

/// At most this many typed bytes are kept across the probe; more typeahead than this is dropped.
const MAX_TYPED: usize = 4096;

/// Incrementally folds terminal replies into a [`Ledger`]. Handles CSI sequences split across reads.
/// Every byte that is not part of a reply is kept, in order, as input the user typed meanwhile
/// ([`ProbeParser::take_typed`], #1013).
#[derive(Default)]
pub struct ProbeParser {
    /// Bytes of an in-progress escape sequence, or leftover partial input between `feed` calls.
    pending: Vec<u8>,
    fenced: bool,
    /// The bytes that were not a reply: keys typed before or during the probe, or after its fence.
    typed: Vec<u8>,
}

impl ProbeParser {
    pub fn new() -> Self {
        ProbeParser::default()
    }

    /// True once the DA1 fence has been seen.
    pub fn is_fenced(&self) -> bool {
        self.fenced
    }

    /// The input the user typed while the probe read the terminal, in order, with the replies taken
    /// out — for the event loop to read as keys. Call once the probe is over: an escape sequence still
    /// incomplete then is input too (a lone `Esc`).
    pub fn take_typed(&mut self) -> Vec<u8> {
        let rest = std::mem::take(&mut self.pending);
        self.keep_typed(&rest);
        std::mem::take(&mut self.typed)
    }

    fn keep_typed(&mut self, bytes: &[u8]) {
        let room = MAX_TYPED.saturating_sub(self.typed.len());
        self.typed
            .extend_from_slice(&bytes[..bytes.len().min(room)]);
    }

    /// Fold a chunk of terminal output into `ledger`. Recognises three CSI replies; everything else
    /// is input the user typed and is kept for [`ProbeParser::take_typed`]:
    ///   * DECRQM report `CSI ? <mode> ; <state> $ y` — state != 0 means the mode is recognised.
    ///   * kitty report  `CSI ? <flags> u`            — the kitty keyboard protocol is present.
    ///   * DA1 report    `CSI ? <params> c`           — the fence; probing is done.
    pub fn feed(&mut self, bytes: &[u8], ledger: &mut Ledger) -> ProbeState {
        self.pending.extend_from_slice(bytes);
        let mut typed = Vec::new();
        let mut i = 0;
        while i < self.pending.len() {
            // Seek the next CSI introducer (ESC '['); anything else is typed input.
            if self.pending[i] != 0x1b {
                typed.push(self.pending[i]);
                i += 1;
                continue;
            }
            match self.pending.get(i + 1) {
                None => break, // ESC at the tail — wait for more bytes.
                Some(&b'[') => {}
                Some(_) => {
                    typed.push(0x1b); // `Esc` or an Alt chord, not a reply
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
            match Self::classify(seq, ledger) {
                Reply::Fence => self.fenced = true,
                Reply::Answer => {}
                Reply::NotOurs => typed.extend_from_slice(&self.pending[i..=j]),
            }
            i = j + 1;
        }
        self.keep_typed(&typed);
        // Drop everything we have consumed or skipped; keep only a trailing partial sequence.
        self.pending.drain(..i);
        if self.fenced {
            ProbeState::Fenced
        } else {
            ProbeState::Awaiting
        }
    }

    /// Classify one complete CSI body (`<params><final>`), update the ledger, and say what it was.
    /// Static (no `&mut self`) so the caller can hold an immutable slice of `pending` while calling it.
    fn classify(seq: &[u8], ledger: &mut Ledger) -> Reply {
        let (&final_byte, params) = match seq.split_last() {
            Some(x) => x,
            None => return Reply::NotOurs,
        };
        match final_byte {
            b'c' if params.first() == Some(&b'?') => {
                // DA1 reply (`CSI ? <attrs> c`). The fence — nothing more will come. A `4` among the
                // attributes advertises SIXEL graphics (F-031 slice 3b / D-053).
                let attrs = &params[1..]; // after the leading '?'
                if attrs.split(|&b| b == b';').any(|p| parse_u16(p) == Some(4)) {
                    ledger.record(
                        Capability::InlineGraphics,
                        Entry {
                            value: CapValue::Graphics(GraphicsProtocol::Sixel),
                            source: Source::Probed,
                            confidence: Confidence::Confirmed,
                        },
                    );
                }
                Reply::Fence
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
                Reply::Answer
            }
            b'y' => {
                // DECRQM report `CSI ? <mode> ; <state> $ y`. `$` is the byte before `y`.
                if params.last() != Some(&b'$') || params.first() != Some(&b'?') {
                    return Reply::NotOurs;
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
                Reply::Answer
            }
            _ => Reply::NotOurs,
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn da1_attribute_4_records_sixel_graphics() {
        // A DA1 reply advertising sixel (attribute `4`): `CSI ? 62 ; 4 ; 22 c` (F-031 slice 3b / D-053).
        let mut l = Ledger::with_defaults();
        let mut p = ProbeParser::new();
        assert_eq!(p.feed(b"\x1b[?62;4;22c", &mut l), ProbeState::Fenced);
        assert_eq!(l.graphics(), GraphicsProtocol::Sixel);
    }

    #[test]
    fn da1_without_attribute_4_leaves_graphics_none() {
        let mut l = Ledger::with_defaults();
        let mut p = ProbeParser::new();
        assert_eq!(p.feed(b"\x1b[?62;22c", &mut l), ProbeState::Fenced);
        assert_eq!(l.graphics(), GraphicsProtocol::None);
    }
}
