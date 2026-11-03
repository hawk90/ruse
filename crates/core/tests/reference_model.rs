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
    AnchorId, AnchorPolicy, Bias, BytePos, Document, DocumentId, Edit, EditList, LineGravity,
    Tracked, Transaction, TransactionOrigin,
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

// ---- Line anchors and marks against a naive line model (#906) -------------------------------------------
//
// The cell model above checks BYTE anchors. Vim's marks, the jumplist and the change list are LINE anchors
// ([`LineGravity`], anchor-store §3.1): they follow whole lines and keep their column through an edit inside
// their line, as nvim's `mark_adjust` moves them. This model keeps the text as a list of lines and each
// mark as `(line, column)`, and applies each change by what it means to lines, written without the
// store's `LineMap`:
//
// - lines inserted at or above a mark's line shift it down;
// - a deleted line takes a `Drop` mark with it, moves a `Keep` mark to the line that took its place (the
//   new last line past the end) at its column, and a `Cursor` mark to column 0 of the line above;
// - an edit inside a mark's line keeps its column, clamped to the new line.
//
// Undo applies the inverse change by the same rules; a mark undo restores (`Tracked::restored`, Vim's
// lowercase marks) then goes back to where it was when the undone change began, and the step keeps the
// value it replaced for redo (Vim swaps `uh_namedm`). The document's own line anchors, plain `Tracked`
// positions and restorable ones all run side by side. Lines are never empty, so every whole-line change has
// one reading (an empty line makes "delete line i" and "delete the `\n` before line i+1" the same bytes).

/// A change, by what it does to lines; selectors are taken modulo the current line count when it applies.
#[derive(Clone, Debug)]
enum LineOp {
    /// Whole lines inserted above line `at` (`O`, `:put!`); `at` may be one past the last line.
    Insert(usize, Vec<Vec<u8>>),
    /// `count` whole lines deleted from line `from` (`dd`), at their line starts.
    Delete(usize, usize),
    /// Line `i` deleted from the `\n` before it to its own `\n` (`dd` on the last line).
    DeleteAfterNewline(usize),
    /// A line opened below line `i` (`o`): `\n` plus the text, at the end of line `i`.
    OpenBelow(usize, Vec<u8>),
    /// Inside line `i`: replace `del` bytes at column `at` with `ins` (no newline).
    InLine(usize, usize, usize, Vec<u8>),
    /// A mark at line, column (taken modulo the line length + 1), followed as `gravity`; `kind` 0 is a
    /// document line anchor, 1 a plain `Tracked`, 2 a `Tracked` undo restores.
    Mark(usize, usize, LineGravity, u8),
    Undo,
    Redo,
}

/// What a change means to lines (the model's own vocabulary, inverse included).
#[derive(Clone, Debug)]
enum Lines {
    Insert(usize, Vec<Vec<u8>>),
    Delete(usize, usize),
    InLine(usize, Vec<u8>),
}

#[derive(Clone, Copy, Debug)]
struct MarkModel {
    pos: Option<(usize, usize)>,
    gravity: LineGravity,
    kind: u8,
}

#[derive(Default)]
struct LineModel {
    lines: Vec<Vec<u8>>,
    marks: Vec<MarkModel>,
}

impl LineModel {
    fn bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for l in &self.lines {
            out.extend_from_slice(l);
            out.push(b'\n');
        }
        out
    }

    fn line_start(&self, i: usize) -> usize {
        self.lines[..i].iter().map(|l| l.len() + 1).sum()
    }

    fn offset(&self, (l, c): (usize, usize)) -> usize {
        self.line_start(l) + c
    }

    /// Apply a change to the text and every mark; returns its inverse.
    fn apply(&mut self, ch: &Lines) -> Lines {
        match ch {
            Lines::Insert(at, new) => {
                let k = new.len();
                for m in &mut self.marks {
                    if let Some((l, _)) = &mut m.pos {
                        if *l >= *at {
                            *l += k;
                        }
                    }
                }
                self.lines.splice(at..at, new.iter().cloned());
                Lines::Delete(*at, k)
            }
            Lines::Delete(from, count) => {
                let (from, count) = (*from, *count);
                let gone: Vec<Vec<u8>> = self.lines.drain(from..from + count).collect();
                let n = self.lines.len();
                for m in &mut self.marks {
                    let Some((l, c)) = m.pos else { continue };
                    m.pos = if l < from {
                        Some((l, c))
                    } else if l >= from + count {
                        Some((l - count, c))
                    } else {
                        match m.gravity {
                            LineGravity::Drop => None,
                            LineGravity::Keep => {
                                let to = from.min(n - 1);
                                Some((to, c.min(self.lines[to].len())))
                            }
                            LineGravity::Cursor => Some((from.saturating_sub(1), 0)),
                        }
                    };
                }
                Lines::Insert(from, gone)
            }
            Lines::InLine(i, text) => {
                let old = std::mem::replace(&mut self.lines[*i], text.clone());
                let len = text.len();
                for m in &mut self.marks {
                    if let Some((l, c)) = &mut m.pos {
                        if *l == *i {
                            *c = (*c).min(len);
                        }
                    }
                }
                Lines::InLine(*i, old)
            }
        }
    }
}

fn line_op() -> impl Strategy<Value = LineOp> {
    let ch = prop_oneof![Just(b'a'), Just(b'b'), Just(b' ')];
    let line = prop::collection::vec(ch.clone(), 1..4);
    let gravity = prop_oneof![
        Just(LineGravity::Drop),
        Just(LineGravity::Keep),
        Just(LineGravity::Cursor)
    ];
    prop_oneof![
        2 => (any::<usize>(), prop::collection::vec(line.clone(), 1..3)).prop_map(|(a, l)| LineOp::Insert(a, l)),
        2 => (any::<usize>(), 1usize..3).prop_map(|(f, c)| LineOp::Delete(f, c)),
        1 => any::<usize>().prop_map(LineOp::DeleteAfterNewline),
        1 => (any::<usize>(), line).prop_map(|(i, l)| LineOp::OpenBelow(i, l)),
        2 => (any::<usize>(), any::<usize>(), 0usize..3, prop::collection::vec(ch, 0..3))
            .prop_map(|(i, a, d, s)| LineOp::InLine(i, a, d, s)),
        3 => (any::<usize>(), any::<usize>(), gravity, 0u8..3).prop_map(|(l, c, g, k)| LineOp::Mark(l, c, g, k)),
        2 => Just(LineOp::Undo),
        1 => Just(LineOp::Redo),
    ]
}

/// One done (or undone) step of the line model: the change, its inverse, and the restorable marks saved
/// for the swap (`None` when there were none when it was made, so the document saved nothing).
struct LineStep {
    fwd: Lines,
    inv: Lines,
    saved: Option<Vec<Option<(usize, usize)>>>,
}

/// Replay `ch` as an undo or redo step does (`Document::replay`): note the restorable marks, apply the
/// change, put each back where `saved` holds it (a mark the step saw unset stays as it is), and keep the
/// noted values in `saved` for the way back. A step that saved nothing swaps nothing.
fn replay(model: &mut LineModel, ch: &Lines, saved: &mut Option<Vec<Option<(usize, usize)>>>) {
    let before: Vec<Option<(usize, usize)>> = model
        .marks
        .iter()
        .filter(|m| m.kind == 2)
        .map(|m| m.pos)
        .collect();
    model.apply(ch);
    let Some(s) = saved else { return };
    let restore = model.marks.iter_mut().filter(|m| m.kind == 2);
    for (m, p) in restore.zip(s.iter()) {
        if p.is_some() {
            m.pos = *p;
        }
    }
    *s = before;
}

proptest! {
    #![proptest_config(cases(256))]

    /// Line anchors and marks match the line model after every step (anchor-store §3.1, #686, #906).
    #[test]
    fn line_anchors_match_the_line_model(
        init in prop::collection::vec(prop::collection::vec(prop_oneof![Just(b'a'), Just(b'b')], 1..4), 1..6),
        ops in prop::collection::vec(line_op(), 1..40),
    ) {
        let mut model = LineModel { lines: init, marks: Vec::new() };
        let mut doc = Document::new(DocumentId(1), model.bytes());
        let mut anchors: Vec<(usize, AnchorId)> = Vec::new();
        let mut tracked: Vec<(usize, Tracked)> = Vec::new();
        let mut done: Vec<LineStep> = Vec::new();
        let mut undone: Vec<LineStep> = Vec::new();

        for op in &ops {
            let n = model.lines.len();
            // The change as bytes and as lines, or `None` for the steps that are not a new change.
            let change: Option<(Edit, Lines)> = match op {
                LineOp::Insert(a, l) => {
                    let at = a % (n + 1);
                    let mut ins = Vec::new();
                    for x in l {
                        ins.extend_from_slice(x);
                        ins.push(b'\n');
                    }
                    Some((Edit::insert(model.line_start(at), ins), Lines::Insert(at, l.clone())))
                }
                LineOp::Delete(f, c) => {
                    if n == 1 {
                        continue;
                    }
                    let from = f % n;
                    let count = (*c).min(n - from).min(n - 1);
                    let (s, e) = (model.line_start(from), model.line_start(from + count));
                    Some((Edit::delete(s, e - s), Lines::Delete(from, count)))
                }
                LineOp::DeleteAfterNewline(i) => {
                    if n == 1 {
                        continue;
                    }
                    let i = 1 + i % (n - 1);
                    let s = model.line_start(i) - 1;
                    Some((Edit::delete(s, model.lines[i].len() + 1), Lines::Delete(i, 1)))
                }
                LineOp::OpenBelow(i, l) => {
                    let i = i % n;
                    let mut ins = vec![b'\n'];
                    ins.extend_from_slice(l);
                    let at = model.line_start(i) + model.lines[i].len();
                    Some((Edit::insert(at, ins), Lines::Insert(i + 1, vec![l.clone()])))
                }
                LineOp::InLine(i, a, d, s) => {
                    let i = i % n;
                    let line = &model.lines[i];
                    let at = a % (line.len() + 1);
                    let del = (*d).min(line.len() - at);
                    let mut text = line[..at].to_vec();
                    text.extend_from_slice(s);
                    text.extend_from_slice(&line[at + del..]);
                    if text.is_empty() || (del == 0 && s.is_empty()) {
                        continue;
                    }
                    let e = Edit::replace(model.line_start(i) + at, del, s.clone());
                    Some((e, Lines::InLine(i, text)))
                }
                LineOp::Mark(l, c, g, k) => {
                    let l = l % n;
                    let c = c % (model.lines[l].len() + 1);
                    let at = model.offset((l, c));
                    let idx = model.marks.len();
                    model.marks.push(MarkModel { pos: Some((l, c)), gravity: *g, kind: *k });
                    match k {
                        0 => anchors.push((idx, doc.create_line_anchor(BytePos(at), *g))),
                        1 => tracked.push((idx, Tracked::line(Some(at), *g))),
                        _ => tracked.push((idx, Tracked::line(Some(at), *g).restored())),
                    }
                    None
                }
                LineOp::Undo => {
                    let mut t: Vec<Tracked> = tracked.iter().map(|(_, t)| *t).collect();
                    let moved = doc.undo_tracked(BytePos(0), &mut t).is_some();
                    for (slot, v) in tracked.iter_mut().zip(t) {
                        slot.1 = v;
                    }
                    prop_assert_eq!(moved, !done.is_empty(), "undo availability");
                    if let Some(mut step) = done.pop() {
                        replay(&mut model, &step.inv, &mut step.saved);
                        undone.push(step);
                    }
                    None
                }
                LineOp::Redo => {
                    let mut t: Vec<Tracked> = tracked.iter().map(|(_, t)| *t).collect();
                    let moved = doc.redo_tracked(BytePos(0), &mut t).is_some();
                    for (slot, v) in tracked.iter_mut().zip(t) {
                        slot.1 = v;
                    }
                    prop_assert_eq!(moved, !undone.is_empty(), "redo availability");
                    if let Some(mut step) = undone.pop() {
                        replay(&mut model, &step.fwd, &mut step.saved);
                        done.push(step);
                    }
                    None
                }
            };

            if let Some((edit, lines)) = change {
                let saved_marks: Vec<Option<(usize, usize)>> =
                    model.marks.iter().filter(|m| m.kind == 2).map(|m| m.pos).collect();
                let saved = (!saved_marks.is_empty()).then_some(saved_marks);
                let list = EditList::new(vec![edit]).expect("one edit is a valid list");
                let mut t: Vec<Tracked> = tracked.iter().map(|(_, t)| *t).collect();
                doc.sync_undo();
                doc.apply_tracked(
                    Transaction::new(doc.revision(), list, TransactionOrigin::UserInput),
                    &mut t,
                )
                .expect("an in-range edit on the current revision applies");
                for (slot, v) in tracked.iter_mut().zip(t) {
                    slot.1 = v;
                }
                let inv = model.apply(&lines);
                done.push(LineStep { fwd: lines, inv, saved });
                undone.clear();
            }

            doc.assert_invariants();
            let want = model.bytes();
            prop_assert_eq!(doc.bytes(), want.as_slice(), "bytes after {:?}", op);
            for (i, id) in &anchors {
                let m = model.marks[*i];
                let r = doc.resolve_anchor(*id).expect("a line anchor stays live");
                match m.pos {
                    None => prop_assert!(r.invalidated, "anchor {} ({:?}) survived its line after {:?}", i, m, op),
                    Some(p) => {
                        prop_assert!(!r.invalidated, "anchor {} ({:?}) dropped after {:?}", i, m, op);
                        prop_assert_eq!(r.offset.0, model.offset(p), "anchor {} {:?} after {:?}", i, m, op);
                    }
                }
            }
            for (i, t) in &tracked {
                let m = model.marks[*i];
                prop_assert_eq!(
                    t.pos,
                    m.pos.map(|p| model.offset(p)),
                    "tracked mark {} {:?} after {:?}", i, m, op
                );
            }
        }
    }
}
