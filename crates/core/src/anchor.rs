//! The anchor store — the single primitive every long-lived position rests on (C-ANCHOR, D-023).
//!
//! Faithful to [`anchor-store.md`]: the [`Bias`] truth table (§2), the total per-edit update rule (§3)
//! including the span-delete collapse and [`AnchorPolicy`], and the batched `O(A + E)` sweep (§4) that
//! meets **INV-ANCHOR** ("not O(anchors × edits)"). Anchors are generation-checked handles
//! (**INV-HANDLE**); a stale id is an assert, never a silent wrong answer.

use crate::edit::EditList;
use crate::pos::BytePos;

/// Which side of a boundary an anchor clings to when an insertion lands exactly at its offset
/// (anchor-store §2; extmark gravity).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Bias {
    /// Inserted text goes to the anchor's **right**; the offset is unchanged. (`right_gravity=false`;
    /// a range's end that must not grow.)
    Before,
    /// Inserted text goes to the anchor's **left**; the offset advances past it. (`right_gravity=true`,
    /// the default; a plain cursor/mark, or a range's start.)
    After,
}

/// What happens to an anchor whose surrounding span is deleted (anchor-store §3 span-delete): both
/// units its gap sat between are gone, so it collapses to the replacement start; the policy decides
/// whether it stays silently live or is flagged for its owner to drop.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AnchorPolicy {
    /// Keep it live at the collapse point (a caret lands where the text was removed).
    Clamp,
    /// Keep it live but set `invalidated`, so a decoration/diagnostic owner can drop it.
    Invalidate,
}

/// A generation-checked handle into the store (INV-HANDLE). A freed slot bumps its generation, so a
/// stale `AnchorId` mismatches and is caught, never dereferenced to the wrong anchor.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct AnchorId {
    slot: u32,
    gen: u32,
}

/// An anchor resolved to a concrete byte position at some revision (anchor-store §5).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Resolved {
    pub offset: BytePos,
    /// True iff a span-delete collapsed this anchor and its policy was [`AnchorPolicy::Invalidate`].
    pub invalidated: bool,
}

#[derive(Clone, Copy, Debug)]
struct Anchor {
    offset: usize,
    bias: Bias,
    policy: AnchorPolicy,
    invalidated: bool,
}

struct Slot {
    gen: u32,
    anchor: Option<Anchor>,
}

/// One authoritative per-document anchor store (anchor-store G1). Holds every cursor, selection edge,
/// mark, diagnostic and decoration position; no subsystem keeps its own offset bookkeeping.
#[derive(Default)]
pub struct AnchorStore {
    slots: Vec<Slot>,
    free: Vec<u32>,
}

impl AnchorStore {
    /// A new, empty store.
    #[must_use]
    pub fn new() -> AnchorStore {
        AnchorStore::default()
    }

    /// Create an anchor at `offset` (bytes) with the given bias and span-delete policy.
    pub fn insert(&mut self, offset: usize, bias: Bias, policy: AnchorPolicy) -> AnchorId {
        let anchor = Anchor {
            offset,
            bias,
            policy,
            invalidated: false,
        };
        if let Some(slot) = self.free.pop() {
            let s = &mut self.slots[slot as usize];
            s.anchor = Some(anchor);
            AnchorId { slot, gen: s.gen }
        } else {
            let slot = self.slots.len() as u32;
            self.slots.push(Slot {
                gen: 0,
                anchor: Some(anchor),
            });
            AnchorId { slot, gen: 0 }
        }
    }

    /// Remove an anchor, freeing its slot (its generation is bumped so the old id is detectably stale).
    /// Returns whether the id was live.
    pub fn remove(&mut self, id: AnchorId) -> bool {
        match self.slots.get_mut(id.slot as usize) {
            Some(s) if s.gen == id.gen && s.anchor.is_some() => {
                s.anchor = None;
                s.gen = s.gen.wrapping_add(1);
                self.free.push(id.slot);
                true
            }
            _ => false,
        }
    }

    fn get(&self, id: AnchorId) -> Option<&Anchor> {
        match self.slots.get(id.slot as usize) {
            Some(s) if s.gen == id.gen => s.anchor.as_ref(),
            _ => None,
        }
    }

    /// Whether `id` currently refers to a live anchor.
    #[must_use]
    pub fn contains(&self, id: AnchorId) -> bool {
        self.get(id).is_some()
    }

    /// Resolve `id` to its current position. A stale/freed id is an invariant violation (INV-HANDLE)
    /// and panics; use [`AnchorStore::try_resolve`] where absence is a legitimate outcome.
    #[must_use]
    pub fn resolve(&self, id: AnchorId) -> Resolved {
        self.try_resolve(id)
            .expect("resolve of a stale AnchorId (INV-HANDLE)")
    }

    /// Resolve `id`, or `None` if it is not live.
    #[must_use]
    pub fn try_resolve(&self, id: AnchorId) -> Option<Resolved> {
        self.get(id).map(|a| Resolved {
            offset: BytePos(a.offset),
            invalidated: a.invalidated,
        })
    }

    /// Number of live anchors.
    #[must_use]
    pub fn len(&self) -> usize {
        self.slots.iter().filter(|s| s.anchor.is_some()).count()
    }

    /// Whether the store holds no live anchors.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.slots.iter().all(|s| s.anchor.is_none())
    }

    /// Every live anchor as `(id, resolved)`, ascending by offset — the material a snapshot freezes
    /// into its anchor index.
    #[must_use]
    pub fn resolved(&self) -> Vec<(AnchorId, Resolved)> {
        let mut out: Vec<(AnchorId, Resolved)> = self
            .slots
            .iter()
            .enumerate()
            .filter_map(|(i, s)| {
                s.anchor.map(|a| {
                    (
                        AnchorId {
                            slot: i as u32,
                            gen: s.gen,
                        },
                        Resolved {
                            offset: BytePos(a.offset),
                            invalidated: a.invalidated,
                        },
                    )
                })
            })
            .collect();
        out.sort_by_key(|(_, r)| r.offset.0);
        out
    }

    /// Carry every live anchor across one transaction's whole disjoint edit set at once (anchor-store
    /// §4). Cost is `O(A log A + E)` here (the store is sorted per apply rather than kept ordered) —
    /// still off the `O(A × E)` the invariant forbids; keeping the store permanently offset-ordered is
    /// the documented optimization.
    pub fn apply_edits(&mut self, edits: &EditList) {
        let es = edits.edits();
        if es.is_empty() {
            return;
        }
        // Live slot indices, ascending by offset — the sweep visits anchors in this order.
        let mut live: Vec<usize> = (0..self.slots.len())
            .filter(|&i| self.slots[i].anchor.is_some())
            .collect();
        live.sort_by_key(|&i| self.slots[i].anchor.expect("live").offset);

        let mut delta: isize = 0; // cumulative delta of all edits fully left of the current anchor
        let mut ei = 0usize;
        for si in live {
            let a = self.slots[si].anchor.as_mut().expect("live");
            let off = a.offset;
            // Advance past edits that end strictly before this anchor; fold their delta into the carry.
            while ei < es.len() && off > es[ei].end() {
                delta += es[ei].delta();
                ei += 1;
            }
            let new_off = if ei < es.len() && es[ei].pos.0 <= off && off <= es[ei].end() {
                let e = &es[ei];
                let (pos, end, insn) = (e.pos.0, e.end(), e.ins.len());
                let local = if off == pos {
                    if a.bias == Bias::Before {
                        pos
                    } else {
                        pos + insn
                    }
                } else if off == end {
                    if a.bias == Bias::After {
                        pos + insn
                    } else {
                        pos
                    }
                } else {
                    // strictly inside a deleted span → collapse to the replacement start
                    if a.policy == AnchorPolicy::Invalidate {
                        a.invalidated = true;
                    }
                    pos
                };
                (local as isize + delta) as usize
            } else {
                // between edits: shift by the carried delta only
                (off as isize + delta) as usize
            };
            a.offset = new_off;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::edit::{Edit, EditList};

    fn at(store: &AnchorStore, id: AnchorId) -> usize {
        store.resolve(id).offset.0
    }

    #[test]
    fn bias_truth_table_at_insertion_point() {
        // Two anchors sharing offset 3; an insertion of 2 bytes lands exactly at 3.
        let mut store = AnchorStore::new();
        let before = store.insert(3, Bias::Before, AnchorPolicy::Clamp);
        let after = store.insert(3, Bias::After, AnchorPolicy::Clamp);
        let edits = EditList::new(vec![Edit::insert(3, b"XY".to_vec())]).unwrap();
        store.apply_edits(&edits);
        // Before clings left (unchanged); After clings right (advances past the inserted bytes).
        assert_eq!(at(&store, before), 3, "Before bias must not grow");
        assert_eq!(
            at(&store, after),
            5,
            "After bias advances past inserted text"
        );
    }

    #[test]
    fn anchor_shifts_when_edit_is_left_of_it() {
        let mut store = AnchorStore::new();
        let a = store.insert(10, Bias::After, AnchorPolicy::Clamp);
        // insert 3 bytes at 2 (wholly left) → anchor shifts by +3.
        store.apply_edits(&EditList::new(vec![Edit::insert(2, b"abc".to_vec())]).unwrap());
        assert_eq!(at(&store, a), 13);
        // delete 4 bytes at 0 (wholly left) → shifts by -4.
        store.apply_edits(&EditList::new(vec![Edit::delete(0, 4)]).unwrap());
        assert_eq!(at(&store, a), 9);
    }

    #[test]
    fn span_delete_clamp_vs_invalidate() {
        let mut store = AnchorStore::new();
        let clamp = store.insert(4, Bias::After, AnchorPolicy::Clamp);
        let inval = store.insert(4, Bias::After, AnchorPolicy::Invalidate);
        // delete [2,6): both anchors sit strictly inside the deleted span → collapse to pos=2.
        store.apply_edits(&EditList::new(vec![Edit::delete(2, 4)]).unwrap());
        let rc = store.resolve(clamp);
        let ri = store.resolve(inval);
        assert_eq!(rc.offset.0, 2);
        assert_eq!(ri.offset.0, 2);
        assert!(!rc.invalidated, "Clamp stays silently live");
        assert!(ri.invalidated, "Invalidate flags the collapsed anchor");
    }

    #[test]
    fn boundary_at_end_of_deleted_span() {
        // anchor at pos+del (the right edge of a deletion): After -> collapse start, Before -> start.
        let mut store = AnchorStore::new();
        let a_after = store.insert(6, Bias::After, AnchorPolicy::Clamp);
        let a_before = store.insert(6, Bias::Before, AnchorPolicy::Clamp);
        // delete [2,6), no insertion: pos=2, end=6, insn=0.
        store.apply_edits(&EditList::new(vec![Edit::delete(2, 4)]).unwrap());
        assert_eq!(
            at(&store, a_after),
            2,
            "After at end of pure deletion collapses to start"
        );
        assert_eq!(
            at(&store, a_before),
            2,
            "Before at end of deletion also lands at start"
        );
    }

    // --- batch ≡ sequential (anchor-store §4 property) ---

    /// The single-edit rule (§3) in one coordinate frame; the reference for the batched sweep.
    fn map1(off: usize, e: &Edit, bias: Bias, policy: AnchorPolicy) -> (usize, bool) {
        let (pos, end, insn, delta) = (e.pos.0, e.end(), e.ins.len(), e.delta());
        if off < pos {
            (off, false)
        } else if off > end {
            ((off as isize + delta) as usize, false)
        } else if off == pos {
            (
                if bias == Bias::Before {
                    pos
                } else {
                    pos + insn
                },
                false,
            )
        } else if off == end {
            (if bias == Bias::After { pos + insn } else { pos }, false)
        } else {
            (pos, policy == AnchorPolicy::Invalidate)
        }
    }

    /// Fold an anchor through a disjoint edit set left-to-right, shifting each edit into the frame left
    /// by prior deltas — the sequential equal of one batched apply.
    fn sequential(
        mut off: usize,
        bias: Bias,
        policy: AnchorPolicy,
        edits: &[Edit],
    ) -> (usize, bool) {
        let mut cum: isize = 0;
        let mut invalidated = false;
        for e in edits {
            let shifted = Edit {
                pos: BytePos((e.pos.0 as isize + cum) as usize),
                del: e.del,
                ins: e.ins.clone(),
            };
            let (n, inv) = map1(off, &shifted, bias, policy);
            off = n;
            invalidated |= inv;
            cum += e.delta();
        }
        (off, invalidated)
    }

    #[test]
    fn batch_equals_sequential_fixed_cases() {
        // three disjoint edits; an anchor between the second and third.
        let edits = vec![
            Edit::insert(1, b"AA".to_vec()),
            Edit::delete(5, 2),
            Edit::replace(9, 1, b"ZZZ".to_vec()),
        ];
        for &off in &[0usize, 1, 3, 5, 7, 9, 10, 12] {
            for bias in [Bias::Before, Bias::After] {
                let mut store = AnchorStore::new();
                let id = store.insert(off, bias, AnchorPolicy::Clamp);
                store.apply_edits(&EditList::new(edits.clone()).unwrap());
                let got = at(&store, id);
                let (want, _) = sequential(
                    off,
                    bias,
                    AnchorPolicy::Clamp,
                    EditList::new(edits.clone()).unwrap().edits(),
                );
                assert_eq!(got, want, "off={off} bias={bias:?}");
            }
        }
    }

    #[test]
    fn batch_equals_sequential_randomized() {
        // Deterministic LCG — no dependency, reproducible.
        let mut seed: u64 = 0x9E3779B97F4A7C15;
        let mut rng = || {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (seed >> 33) as usize
        };
        for _ in 0..500 {
            let doc_len = 40;
            // build a random disjoint, sorted edit set by walking left→right leaving gaps.
            let mut edits = Vec::new();
            let mut cursor = rng() % 4;
            while cursor < doc_len {
                let del = rng() % 4;
                if cursor + del > doc_len {
                    break;
                }
                let ins_len = rng() % 4;
                let ins = vec![b'x'; ins_len];
                if del > 0 || ins_len > 0 {
                    edits.push(Edit {
                        pos: BytePos(cursor),
                        del,
                        ins,
                    });
                }
                cursor += del + 1 + (rng() % 3); // advance past the interval + a gap
            }
            let Ok(list) = EditList::new(edits.clone()) else {
                continue;
            };
            for _ in 0..5 {
                let off = rng() % (doc_len + 1);
                let bias = if rng() % 2 == 0 {
                    Bias::Before
                } else {
                    Bias::After
                };
                let policy = if rng() % 2 == 0 {
                    AnchorPolicy::Clamp
                } else {
                    AnchorPolicy::Invalidate
                };
                let mut store = AnchorStore::new();
                let id = store.insert(off, bias, policy);
                store.apply_edits(&list);
                let got = store.resolve(id);
                let (want_off, want_inv) = sequential(off, bias, policy, list.edits());
                assert_eq!(
                    got.offset.0,
                    want_off,
                    "off={off} bias={bias:?} edits={:?}",
                    list.edits()
                );
                assert_eq!(got.invalidated, want_inv, "invalidated mismatch off={off}");
            }
        }
    }

    #[test]
    fn stale_id_is_not_resolvable() {
        let mut store = AnchorStore::new();
        let id = store.insert(1, Bias::After, AnchorPolicy::Clamp);
        assert!(store.remove(id));
        assert!(
            store.try_resolve(id).is_none(),
            "freed id must not resolve (INV-HANDLE)"
        );
        assert!(!store.contains(id));
    }
}
