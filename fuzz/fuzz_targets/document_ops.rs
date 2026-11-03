//! The document kernel under arbitrary operations, coverage-guided (#920): transactions, undo, redo,
//! chronological steps, saves, anchors and snapshots, with the invariant walker (#906) after each. The
//! byte-exact reference model is `crates/core/tests/reference_model.rs`; this target looks for states the
//! walker rejects or a panic, over far longer and stranger sequences than the proptest generates.

#![no_main]

use libfuzzer_sys::fuzz_target;
use ruse_core::{
    AnchorPolicy, Bias, BytePos, Document, DocumentId, Edit, EditList, LineGravity, Transaction,
    TransactionOrigin,
};

fuzz_target!(|data: &[u8]| {
    let mut doc = Document::new(DocumentId(1), b"ab\ncd\n".to_vec());
    let mut anchors = Vec::new();
    let mut it = data.chunks(4);
    for _ in 0..256 {
        let Some(op) = it.next() else { break };
        let [k, a, b, c] = [0, 1, 2, 3].map(|i| op.get(i).copied().unwrap_or(0));
        let len = doc.len();
        match k % 10 {
            0..=3 => {
                // One to three edits, inserting from a small alphabet with a newline and a multibyte char.
                let mut edits = Vec::new();
                for (i, p) in [a, b, c]
                    .iter()
                    .enumerate()
                    .take(usize::from(k / 10 % 3) + 1)
                {
                    let pos = usize::from(*p) % (len + 1);
                    let del = usize::from(p.wrapping_mul(7) >> 5).min(len - pos);
                    let ins = ["", "x", "\n", "가", "yz\n"][(usize::from(*p) + i) % 5];
                    edits.push(Edit::replace(pos, del, ins.as_bytes().to_vec()));
                }
                if let Ok(list) = EditList::new(edits) {
                    if k & 0x80 != 0 {
                        doc.sync_undo();
                    }
                    doc.apply(Transaction::new(
                        doc.revision(),
                        list,
                        TransactionOrigin::UserInput,
                    ))
                    .expect("an in-range list on the current revision applies");
                }
            }
            4 => {
                doc.undo();
            }
            5 => {
                doc.redo();
            }
            6 => {
                doc.undo_chronological(a & 1 == 0);
            }
            7 => doc.mark_saved(),
            8 => {
                let at = BytePos(usize::from(a) % (len + 1));
                let id = match b % 4 {
                    0 => doc.create_anchor(at, Bias::Before, AnchorPolicy::Clamp),
                    1 => doc.create_anchor(at, Bias::After, AnchorPolicy::Invalidate),
                    2 => doc.create_line_anchor(at, LineGravity::Drop),
                    _ => doc.create_line_anchor(at, LineGravity::Keep),
                };
                anchors.push(id);
            }
            _ => {
                if c & 1 == 0 {
                    let _ = doc.snapshot();
                } else if !anchors.is_empty() {
                    let id = anchors.swap_remove(usize::from(a) % anchors.len());
                    doc.remove_anchor(id);
                }
            }
        }
        doc.assert_invariants();
        if doc.len() > 1 << 16 {
            break;
        }
    }
});
