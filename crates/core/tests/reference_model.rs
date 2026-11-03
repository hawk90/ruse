//! Reference-model properties for the document kernel (#906, testing-and-benchmarks.md §1.2).
//!
//! The older properties check totality (in bounds, valid UTF-8, round trips). These run the real
//! [`Document`] next to a naive model written without the store's code — a list of cells, each a byte or
//! an anchor — through random transactions, undo, redo, saves, anchor creation and removal, and compare
//! after every step: the bytes, every anchor's offset and `invalidated` flag (bias and span-delete
//! policy, anchor-store §2–§3), the modified flag against the model's save point, and the whole-state
//! invariant walker ([`Document::assert_invariants`]). Neovim's `marktree_spec.lua` and Emacs's
//! `test-overlay-randomly` are the upstream shape of this test.

use proptest::prelude::*;
use ruse_core::{
    AnchorId, AnchorPolicy, Bias, BytePos, Document, DocumentId, Edit, EditList, Transaction,
    TransactionOrigin,
};

/// `n` cases, scaled by `RUSE_PROPTEST_SCALE` for the scheduled deep tier (#920), with the reject budget
/// scaled alike so a filtering test does not abort a long run.
fn cases(n: u32) -> ProptestConfig {
    let scale = std::env::var("RUSE_PROPTEST_SCALE")
        .ok()
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(1)
        .max(1);
    let base = ProptestConfig::default();
    ProptestConfig {
        cases: n.saturating_mul(scale),
        max_global_rejects: base.max_global_rejects.saturating_mul(scale),
        ..base
    }
}

/// One cell of the model text: a byte, or the gap position of anchor `i` (an index into `Model::anchors`).
#[derive(Clone, Copy, Debug)]
enum Cell {
    Byte(u8),
    Anchor(usize),
}

#[derive(Clone, Debug)]
struct ModelAnchor {
    bias: Bias,
    policy: AnchorPolicy,
    /// Ever sat strictly inside a deleted span.
    inside: bool,
    live: bool,
}

/// The naive model: the text as cells, anchors living between bytes. An edit deletes its bytes (an
/// anchor with a deleted byte on both sides sat inside the span), then inserts its bytes into the gap,
/// after every `Before` anchor there and before every `After` one.
#[derive(Clone, Debug, Default)]
struct Model {
    cells: Vec<Cell>,
    anchors: Vec<ModelAnchor>,
}

impl Model {
    fn new(bytes: &[u8]) -> Model {
        Model {
            cells: bytes.iter().map(|&b| Cell::Byte(b)).collect(),
            anchors: Vec::new(),
        }
    }

    fn bytes(&self) -> Vec<u8> {
        self.cells
            .iter()
            .filter_map(|c| match c {
                Cell::Byte(b) => Some(*b),
                Cell::Anchor(_) => None,
            })
            .collect()
    }

    /// The cell index of the `n`th byte (0-based), or `cells.len()` past the last.
    fn byte_index(&self, n: usize) -> usize {
        let mut seen = 0;
        for (i, c) in self.cells.iter().enumerate() {
            if let Cell::Byte(_) = c {
                if seen == n {
                    return i;
                }
                seen += 1;
            }
        }
        self.cells.len()
    }

    /// The cell index where the gap before byte `n` begins: just past byte `n - 1`.
    fn gap_start(&self, n: usize) -> usize {
        if n == 0 {
            0
        } else {
            self.byte_index(n - 1) + 1
        }
    }

    fn add_anchor(&mut self, offset: usize, bias: Bias, policy: AnchorPolicy) -> usize {
        let i = self.anchors.len();
        self.anchors.push(ModelAnchor {
            bias,
            policy,
            inside: false,
            live: true,
        });
        // A new anchor's place among others at the same offset does not matter: they move alike by bias.
        let at = self.gap_start(offset);
        self.cells.insert(at, Cell::Anchor(i));
        i
    }

    fn remove_anchor(&mut self, i: usize) {
        self.anchors[i].live = false;
        self.cells
            .retain(|c| !matches!(c, Cell::Anchor(j) if *j == i));
    }

    /// One transaction: its edits one by one from the left, each moved into the text the edits before
    /// it leave. That order is the contract for touching edits (#758): an `After` anchor pushed to the
    /// end of one edit's insertion meets the next edit at its start and moves again.
    fn apply(&mut self, list: &EditList) {
        let mut delta = 0isize;
        for e in list.edits() {
            self.apply_one((e.pos.0 as isize + delta) as usize, e);
            delta += e.delta();
        }
    }

    fn apply_one(&mut self, pos: usize, e: &Edit) {
        if e.del > 0 {
            let first = self.byte_index(pos);
            let last = self.byte_index(pos + e.del - 1);
            for c in &self.cells[first..last] {
                if let Cell::Anchor(i) = c {
                    self.anchors[*i].inside = true;
                }
            }
            let mut k = 0;
            self.cells.retain(|c| {
                let keep = !(first <= k && k <= last && matches!(c, Cell::Byte(_)));
                k += 1;
                keep
            });
        }
        // The gap at `pos` is the run of anchor cells between byte `pos - 1` and byte `pos`.
        let lo = self.gap_start(pos);
        let mut hi = lo;
        while matches!(self.cells.get(hi), Some(Cell::Anchor(_))) {
            hi += 1;
        }
        let gap: Vec<Cell> = self.cells[lo..hi].to_vec();
        let mut run: Vec<Cell> = gap
            .iter()
            .copied()
            .filter(|c| matches!(c, Cell::Anchor(i) if self.anchors[*i].bias == Bias::Before))
            .collect();
        run.extend(e.ins.iter().map(|&b| Cell::Byte(b)));
        run.extend(
            gap.iter()
                .copied()
                .filter(|c| matches!(c, Cell::Anchor(i) if self.anchors[*i].bias == Bias::After)),
        );
        self.cells.splice(lo..hi, run);
    }

    /// Anchor `i`'s offset: the bytes before its cell.
    fn offset(&self, i: usize) -> usize {
        let mut n = 0;
        for c in &self.cells {
            match c {
                Cell::Byte(_) => n += 1,
                Cell::Anchor(j) if *j == i => return n,
                Cell::Anchor(_) => {}
            }
        }
        panic!("model anchor {i} is not in the text")
    }
}

#[derive(Clone, Debug)]
enum Op {
    /// One transaction; positions are taken modulo the text length when it applies.
    Edit(Vec<(usize, usize, Vec<u8>)>),
    Undo,
    Redo,
    Save,
    Anchor(usize, Bias, AnchorPolicy),
    Remove(usize),
    Snapshot,
}

fn op() -> impl Strategy<Value = Op> {
    let byte = prop_oneof![Just(b'a'), Just(b'b'), Just(b'\n'), Just(0xC3), Just(0xA9)];
    let ins = prop::collection::vec(byte, 0..4);
    let edit = (any::<usize>(), 0usize..5, ins);
    let bias = prop_oneof![Just(Bias::Before), Just(Bias::After)];
    let policy = prop_oneof![Just(AnchorPolicy::Clamp), Just(AnchorPolicy::Invalidate)];
    prop_oneof![
        6 => prop::collection::vec(edit, 1..4).prop_map(Op::Edit),
        2 => Just(Op::Undo),
        1 => Just(Op::Redo),
        1 => Just(Op::Save),
        2 => (any::<usize>(), bias, policy).prop_map(|(o, b, p)| Op::Anchor(o, b, p)),
        1 => any::<usize>().prop_map(Op::Remove),
        1 => Just(Op::Snapshot),
    ]
}

/// The model's undo history: a linear stack of done and undone steps with a node id each, the way
/// `u`/CTRL-R walk the tree (a new change after an undo starts a branch the redo stack forgets).
#[derive(Default)]
struct History {
    done: Vec<(u64, EditList, EditList)>,
    undone: Vec<(u64, EditList, EditList)>,
    next: u64,
}

impl History {
    fn current(&self) -> u64 {
        self.done.last().map_or(0, |d| d.0)
    }
}

proptest! {
    #![proptest_config(cases(256))]

    /// The document matches the cell model after every step: bytes, anchor offsets and flags, the
    /// modified flag, and the invariant walker.
    #[test]
    fn document_matches_the_cell_model(
        init in prop::collection::vec(prop_oneof![Just(b'a'), Just(b'\n'), Just(b'b')], 0..12),
        ops in prop::collection::vec(op(), 1..40),
    ) {
        let mut doc = Document::new(DocumentId(1), init.clone());
        doc.mark_saved();
        let mut model = Model::new(&init);
        let mut hist = History::default();
        let mut saved = Some(0u64);
        let mut saved_bytes = init.clone();
        let mut ids: Vec<AnchorId> = Vec::new();

        for op in &ops {
            match op {
                Op::Edit(raw) => {
                    let len = doc.len();
                    let edits: Vec<Edit> = raw
                        .iter()
                        .map(|(p, d, ins)| {
                            let p = p % (len + 1);
                            Edit::replace(p, (*d).min(len - p), ins.clone())
                        })
                        .collect();
                    let Ok(list) = EditList::new(edits) else { continue };
                    let old = doc.bytes().to_vec();
                    doc.sync_undo();
                    doc.apply(Transaction::new(doc.revision(), list.clone(), TransactionOrigin::UserInput))
                        .expect("an in-range list on the current revision applies");
                    model.apply(&list);
                    if !list.is_empty() {
                        hist.next += 1;
                        let inv = list.inverse(&old);
                        hist.done.push((hist.next, list, inv));
                        hist.undone.clear();
                    }
                }
                Op::Undo => {
                    let moved = doc.undo().is_some();
                    let step = hist.done.pop();
                    prop_assert_eq!(moved, step.is_some(), "undo availability");
                    if let Some(step) = step {
                        model.apply(&step.2);
                        hist.undone.push(step);
                    }
                }
                Op::Redo => {
                    let moved = doc.redo().is_some();
                    let step = hist.undone.pop();
                    prop_assert_eq!(moved, step.is_some(), "redo availability");
                    if let Some(step) = step {
                        model.apply(&step.1);
                        hist.done.push(step);
                    }
                }
                Op::Save => {
                    doc.mark_saved();
                    saved = Some(hist.current());
                    saved_bytes = doc.bytes().to_vec();
                }
                Op::Anchor(o, b, p) => {
                    let o = o % (doc.len() + 1);
                    ids.push(doc.create_anchor(BytePos(o), *b, *p));
                    model.add_anchor(o, *b, *p);
                }
                Op::Remove(i) => {
                    if ids.is_empty() {
                        continue;
                    }
                    let i = i % ids.len();
                    let was = model.anchors[i].live;
                    prop_assert_eq!(doc.remove_anchor(ids[i]), was, "remove reports liveness");
                    if was {
                        model.remove_anchor(i);
                    }
                }
                Op::Snapshot => {
                    let snap = doc.snapshot();
                    for (i, id) in ids.iter().enumerate() {
                        if model.anchors[i].live {
                            let r = snap.resolve(*id).expect("a live anchor is in the snapshot");
                            prop_assert_eq!(r.offset.0, model.offset(i), "snapshot offset");
                        }
                    }
                }
            }

            doc.assert_invariants();
            let want = model.bytes();
            prop_assert_eq!(doc.bytes(), want.as_slice(), "bytes after {:?}", op);
            for (i, id) in ids.iter().enumerate() {
                let a = &model.anchors[i];
                match doc.resolve_anchor(*id) {
                    None => prop_assert!(!a.live, "anchor {} died", i),
                    Some(r) => {
                        prop_assert!(a.live, "removed anchor {} still resolves", i);
                        prop_assert_eq!(r.offset.0, model.offset(i), "anchor {} {:?} after {:?}", i, a, op);
                        prop_assert_eq!(
                            r.invalidated,
                            a.inside && a.policy == AnchorPolicy::Invalidate,
                            "anchor {} flag {:?} after {:?}", i, a, op
                        );
                    }
                }
            }
            let at_save = saved == Some(hist.current());
            prop_assert_eq!(doc.is_modified(), !at_save, "modified flag after {:?}", op);
            if at_save {
                prop_assert_eq!(doc.bytes(), saved_bytes.as_slice(), "the save point's bytes");
            }
        }
    }
}
