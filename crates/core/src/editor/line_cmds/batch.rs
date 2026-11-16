//! The provenance-agnostic batch edit ([`EditorState::apply_edits`]) that the LSP frontend (and any
//! snippet/macro/AI batch) applies as one undo group under its own origin.

use crate::edit::{Edit, EditList};
use crate::editor::{EditorState, MAX_APPLY_EDITS, MAX_APPLY_EDIT_BYTES};
use crate::pos::BytePos;
use crate::transaction::TransactionOrigin;

impl EditorState {
    /// Apply a set of DISJOINT byte-range replacements as ONE undo group, tagged with the caller-supplied
    /// `origin` — a separate undo unit from user edits (F-005). This is a PROVENANCE-AGNOSTIC batch-edit
    /// primitive: the CORE provides the mechanism, the CALLER states the policy (the LSP frontend passes
    /// [`TransactionOrigin::Lsp`]; a snippet/macro/AI batch would pass its own origin). Each edit is
    /// `(start, end, replacement)` in [`BytePos`] (INV-POS-TYPED: a caller holding UTF-16 or char columns,
    /// as an LSP client does, converts at its side of this boundary). The batch is atomic (#716): if any edit
    /// is reversed, out of range, splits a UTF-8 character or overlaps another, NOTHING is applied and this
    /// returns `false`. The cursor is carried through the edits as a byte anchor that follows the text after
    /// it, so an edit above it moves it with its text. A batch of more than [`MAX_APPLY_EDITS`] edits or
    /// [`MAX_APPLY_EDIT_BYTES`] of replacement text is refused too, so a trace's `ex_edits` record has the
    /// same bound live and in replay (RFC-0020 R6).
    pub fn apply_edits(
        &mut self,
        edits: &[(BytePos, BytePos, String)],
        origin: TransactionOrigin,
    ) -> bool {
        let b = self.doc.bytes();
        let boundary = |p: usize| p == b.len() || (p < b.len() && (b[p] & 0xC0) != 0x80);
        let text_bytes = edits
            .iter()
            .map(|(_, _, t)| t.len())
            .fold(0usize, usize::saturating_add);
        if edits.is_empty()
            || edits.len() > MAX_APPLY_EDITS
            || text_bytes > MAX_APPLY_EDIT_BYTES
            || !edits
                .iter()
                .all(|(s, e, _)| s <= e && e.0 <= b.len() && boundary(s.0) && boundary(e.0))
        {
            return false;
        }
        let mut es: Vec<Edit> = edits
            .iter()
            .map(|(s, e, t)| Edit::replace(*s, e.0 - s.0, t.clone().into_bytes()))
            .collect();
        // The cursor follows the edits as given: it stays before the newline added below.
        let Ok(given) = EditList::new(es.clone()) else {
            return false; // overlapping edits — refuse rather than corrupt the buffer
        };
        let cursor = crate::anchor::map_byte(
            &given,
            BytePos(self.view.cursor),
            crate::anchor::Bias::After,
        )
        .0;
        // The edits address the bytes, so the emptied buffer's text is the empty text they were made
        // against; what they give it ends with the line's newline (RFC-0024 §5), as typing there would.
        if self.makes_the_emptied_line() {
            if let Some(last) = es.iter_mut().rev().find(|e| !e.ins.is_empty()) {
                if last.ins.last() != Some(&b'\n') {
                    last.ins.push(b'\n');
                }
            }
        }
        let Ok(list) = EditList::new(es) else {
            return false;
        };
        if self.apply_ex_edit(list, origin, None).is_err() {
            return false;
        }
        self.view.last_was_edit = true;
        self.set_cursor(BytePos(cursor.min(self.doc.bytes().len())));
        self.settle_offsets();
        true
    }
}
