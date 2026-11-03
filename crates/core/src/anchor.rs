//! The anchor store — the single primitive every long-lived position rests on (C-ANCHOR, D-023).
//!
//! Faithful to [`anchor-store.md`]: the [`Bias`] truth table (§2), the total per-edit update rule (§3)
//! including the span-delete collapse and [`AnchorPolicy`], and the batched `O(A + E)` sweep (§4) that
//! meets **INV-ANCHOR** ("not O(anchors × edits)"). Anchors are generation-checked handles
//! (**INV-HANDLE**); a stale id is an assert, never a silent wrong answer.
//!
//! Besides these BYTE anchors the store holds LINE anchors ([`Track::Line`], anchor-store §3.1): Vim's
//! marks, jumplist and change list follow whole-line inserts and deletes but keep their column through an
//! edit inside their line, which is what nvim's `mark_adjust` does (#686). [`LineMap`] is that rule.

use crate::edit::{Edit, EditList};
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

/// What a LINE anchor does when its whole line is deleted — the three cases of nvim's `mark_adjust`
/// (`ONE_ADJUST`, `ONE_ADJUST_NODEL`, `ONE_ADJUST_CURSOR`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LineGravity {
    /// The position is deleted with its line (a lowercase mark, `` `^ ``): `E20` after.
    Drop,
    /// The position moves to the line that took the deleted line's place, keeping its column (an
    /// uppercase mark, the jumplist, the change list, the Visual marks).
    Keep,
    /// The position moves to column 0 of the line before the deleted ones (another window's cursor).
    Cursor,
}

/// How an anchor follows an edit: by byte with a [`Bias`] (decorations, a caret), or by line with a
/// [`LineGravity`] (Vim marks, anchor-store §3.1).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Track {
    Byte(Bias),
    Line(LineGravity),
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
    /// True iff a span-delete collapsed this anchor and its policy was [`AnchorPolicy::Invalidate`], or
    /// its line was deleted and it is a [`LineGravity::Drop`] line anchor.
    pub invalidated: bool,
}

#[derive(Clone, Copy, Debug)]
struct Anchor {
    /// The offset LESS the shift pending at its rank ([`AnchorStore::shift`]); the absolute offset
    /// while unranked.
    offset: usize,
    track: Track,
    policy: AnchorPolicy,
    invalidated: bool,
    /// Its index in [`AnchorStore::order`], or [`UNRANKED`] until the next normalize.
    rank: u32,
}

const UNRANKED: u32 = u32::MAX;

struct Slot {
    gen: u32,
    anchor: Option<Anchor>,
}

/// A Fenwick tree of pending shifts over anchor ranks: an edit adds its delta from the first rank past
/// it on, in `O(log A)`, instead of rewriting every later anchor (anchor-store §4; #1122 measured the
/// per-edit rebuild).
#[derive(Default)]
struct Shifts(Vec<isize>);

impl Shifts {
    fn reset(&mut self, n: usize) {
        self.0.clear();
        self.0.resize(n, 0);
    }

    /// Add `d` to every rank `>= r`.
    fn add_from(&mut self, r: usize, d: isize) {
        let mut i = r + 1;
        while i <= self.0.len() {
            self.0[i - 1] += d;
            i += i.isolate_lowest_one();
        }
    }

    /// The shift pending at rank `r`.
    fn at(&self, r: usize) -> isize {
        let mut i = r + 1;
        let mut s = 0;
        while i > 0 {
            s += self.0[i - 1];
            i -= i.isolate_lowest_one();
        }
        s
    }
}

/// One authoritative per-document anchor store (anchor-store G1). Holds every cursor, selection edge,
/// mark, diagnostic and decoration position; no subsystem keeps its own offset bookkeeping.
///
/// The store is kept offset-ordered (`order`, by rank) with a Fenwick tree of pending shifts, so an
/// edit costs `O(log A)` plus the anchors on the lines it touches, not a pass over every anchor. An
/// insert or remove only marks the order stale; the next edit re-sorts once (`O(A log A)`), so a batch
/// of creations is paid for once.
#[derive(Default)]
pub struct AnchorStore {
    slots: Vec<Slot>,
    free: Vec<u32>,
    /// Live slot indices by rank, ascending by offset (valid while `!stale`).
    order: Vec<u32>,
    shift: Shifts,
    /// An anchor was inserted or removed since the order was last built.
    stale: bool,
}

/// The single-edit rule (anchor-store §3) for one anchor at `a`, in the frame the edit `[pos, pos+del)
/// → ins` is expressed in: the deleted span collapses to `pos`, then the insertion is placed by bias,
/// exactly as a delete followed by an insert (and as nvim moves an extmark). Returns the new offset and
/// whether the anchor sat strictly inside the deleted span.
fn map_one(a: usize, pos: usize, del: usize, ins: usize, bias: Bias) -> (usize, bool) {
    let end = pos + del;
    if a < pos {
        (a, false)
    } else if a > end {
        (a - del + ins, false)
    } else {
        let inside = pos < a && a < end;
        match bias {
            Bias::Before => (pos, inside),
            Bias::After => (pos + ins, inside),
        }
    }
}

/// The byte rule over a whole edit set for one anchor at `o`: the edits it reaches, in turn, each
/// shifted into the frame the edits before it leave — the same answer as applying them one by one, also
/// for touching edits (#758). Returns the new offset and whether it sat strictly inside a deleted span.
fn map_byte_inside(es: &[Edit], o: usize, bias: Bias) -> (usize, bool) {
    let i = es.partition_point(|e| e.end() < o);
    let mut shift: isize = es[..i].iter().map(Edit::delta).sum();
    let mut cur = (o as isize + shift) as usize;
    let mut inside = false;
    for e in &es[i..] {
        let pos = (e.pos.0 as isize + shift) as usize;
        if cur < pos {
            break;
        }
        let (n, ins) = map_one(cur, pos, e.del, e.ins.len(), bias);
        cur = n;
        inside |= ins;
        shift += e.delta();
    }
    (cur, inside)
}

impl AnchorStore {
    /// A new, empty store.
    #[must_use]
    pub fn new() -> AnchorStore {
        AnchorStore::default()
    }

    /// Create a byte anchor at `offset` (bytes) with the given bias and span-delete policy.
    pub fn insert(&mut self, offset: usize, bias: Bias, policy: AnchorPolicy) -> AnchorId {
        self.insert_tracked(offset, Track::Byte(bias), policy)
    }

    /// Create a LINE anchor at `offset` (anchor-store §3.1): it follows line inserts and deletes, keeps
    /// its column through an edit inside its line, and does `gravity` when its line is deleted.
    pub fn insert_line(&mut self, offset: usize, gravity: LineGravity) -> AnchorId {
        self.insert_tracked(offset, Track::Line(gravity), AnchorPolicy::Clamp)
    }

    fn insert_tracked(&mut self, offset: usize, track: Track, policy: AnchorPolicy) -> AnchorId {
        let anchor = Anchor {
            offset,
            track,
            policy,
            invalidated: false,
            rank: UNRANKED,
        };
        self.stale = true;
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
                self.stale = true;
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

    /// The absolute offset of `a` (its stored offset plus the shift pending at its rank).
    fn offset_of(&self, a: &Anchor) -> usize {
        if a.rank == UNRANKED {
            a.offset
        } else {
            (a.offset as isize + self.shift.at(a.rank as usize)) as usize
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
            offset: BytePos(self.offset_of(a)),
            invalidated: a.invalidated,
        })
    }

    /// Number of live anchors.
    #[must_use]
    pub fn len(&self) -> usize {
        self.slots.len() - self.free.len()
    }

    /// Whether the store holds no live anchors.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Every live anchor as `(id, resolved)`, ascending by offset — the material a snapshot freezes
    /// into its anchor index.
    #[must_use]
    pub fn resolved(&self) -> Vec<(AnchorId, Resolved)> {
        let entry = |i: usize| {
            let s = &self.slots[i];
            s.anchor.map(|a| {
                (
                    AnchorId {
                        slot: i as u32,
                        gen: s.gen,
                    },
                    Resolved {
                        offset: BytePos(self.offset_of(&a)),
                        invalidated: a.invalidated,
                    },
                )
            })
        };
        if self.stale {
            let mut out: Vec<_> = (0..self.slots.len()).filter_map(entry).collect();
            out.sort_by_key(|(_, r)| r.offset.0);
            out
        } else {
            self.order
                .iter()
                .filter_map(|&s| entry(s as usize))
                .collect()
        }
    }

    /// Rebuild the order after inserts/removes: fold every pending shift in and sort once.
    fn normalize(&mut self) {
        let mut live: Vec<(usize, u32)> = Vec::with_capacity(self.len());
        for i in 0..self.slots.len() {
            if let Some(a) = self.slots[i].anchor {
                live.push((self.offset_of(&a), i as u32));
            }
        }
        live.sort_unstable();
        self.order.clear();
        for (r, &(off, slot)) in live.iter().enumerate() {
            let a = self.slots[slot as usize].anchor.as_mut().expect("live");
            a.offset = off;
            a.rank = r as u32;
            self.order.push(slot);
        }
        self.shift.reset(self.order.len());
        self.stale = false;
    }

    /// The absolute offset of the anchor at rank `r`.
    fn at_rank(&self, r: usize) -> usize {
        let a = self.slots[self.order[r] as usize]
            .anchor
            .expect("ranked anchors are live");
        (a.offset as isize + self.shift.at(r)) as usize
    }

    /// Carry every live anchor across one transaction's whole disjoint edit set (anchor-store §4). With
    /// no text to read, a line anchor moves like an `After` byte anchor; [`AnchorStore::apply_change`]
    /// gives it its line rule.
    pub fn apply_edits(&mut self, edits: &EditList) {
        self.apply(edits, None);
    }

    /// Carry every live anchor across one change: the byte anchors by the byte rule, the line anchors by
    /// [`LineMap`] over the text before (`old`) and after (`new`) it. A [`LineGravity::Drop`] anchor whose
    /// line was deleted stays resolvable at the edit, flagged `invalidated`.
    pub fn apply_change(&mut self, old: &[u8], new: &[u8], edits: &EditList) {
        self.apply(edits, Some(LineMap::new(old, new, edits)));
    }

    /// The edit cost is `O(log² A)` to find each touched region plus the anchors in it: the region is
    /// the edits' lines (with one line either side, where a line anchor can land) or, without text, the
    /// edits' closed spans. Regions go right to left, so the order is never searched across a region
    /// already moved; each region's anchors are mapped from their old offsets, re-sorted among
    /// themselves, and every rank past the region takes its delta from the Fenwick tree.
    fn apply(&mut self, edits: &EditList, map: Option<LineMap<'_>>) {
        let es = edits.edits();
        if es.is_empty() || self.is_empty() {
            return;
        }
        if self.stale {
            self.normalize();
        }
        // The touched regions, in old offsets (inclusive), merged when they meet, with their delta.
        let mut regions: Vec<(usize, usize, isize)> = Vec::new();
        for e in es {
            let (ws, we) = match &map {
                Some(m) => {
                    let ls = crate::pos::line_start(m.old, e.pos.0);
                    let ws = crate::pos::line_start(m.old, ls.saturating_sub(1));
                    let le = crate::pos::line_end(m.old, e.end());
                    (ws, crate::pos::line_end(m.old, le + 1))
                }
                None => (e.pos.0, e.end()),
            };
            match regions.last_mut() {
                Some(last) if ws <= last.1 => {
                    last.1 = last.1.max(we);
                    last.2 += e.delta();
                }
                _ => regions.push((ws, we, e.delta())),
            }
        }
        // Each region's anchors are stored LESS the deltas of the regions left of it (`left`), which
        // the later steps add from their ranks on.
        let mut left: isize = regions.iter().map(|r| r.2).sum();
        let n = self.order.len();
        for &(ws, we, delta) in regions.iter().rev() {
            left -= delta;
            let lo = partition(n, |r| self.at_rank(r) < ws);
            let hi = lo + partition(n - lo, |r| self.at_rank(lo + r) <= we);
            let mut moved: Vec<(usize, u32, bool)> = (lo..hi)
                .map(|r| {
                    let slot = self.order[r];
                    let a = self.slots[slot as usize].anchor.expect("live");
                    let o = self.at_rank(r);
                    let (to, gone) = match (a.track, &map) {
                        (Track::Line(g), Some(m)) => match m.map(o, g) {
                            Some(p) => (p, false),
                            None => (m.map(o, LineGravity::Keep).expect("Keep never drops"), true),
                        },
                        (Track::Line(_), None) => (map_byte_inside(es, o, Bias::After).0, false),
                        (Track::Byte(bias), _) => {
                            let (p, inside) = map_byte_inside(es, o, bias);
                            (p, inside && a.policy == AnchorPolicy::Invalidate)
                        }
                    };
                    (to, slot, gone)
                })
                .collect();
            moved.sort_by_key(|m| m.0);
            for (i, &(to, slot, gone)) in moved.iter().enumerate() {
                let r = lo + i;
                self.order[r] = slot;
                let pending = self.shift.at(r);
                let a = self.slots[slot as usize].anchor.as_mut().expect("live");
                a.rank = r as u32;
                a.offset = (to as isize - left - pending) as usize;
                a.invalidated |= gone;
            }
            self.shift.add_from(hi, delta);
            if cfg!(debug_assertions) {
                // The order holds across the region's edges (anchor-store §3 monotonicity).
                for r in lo.saturating_sub(1)..(hi + 1).min(n) {
                    if r + 1 < n {
                        debug_assert!(
                            self.at_rank(r) <= self.at_rank(r + 1),
                            "anchor order inverted at rank {r}"
                        );
                    }
                }
            }
        }
    }
}

/// The count of leading `r` in `0..n` with `pred(r)` (a monotone predicate).
fn partition(n: usize, pred: impl Fn(usize) -> bool) -> usize {
    let (mut lo, mut hi) = (0, n);
    while lo < hi {
        let mid = (lo + hi) / 2;
        if pred(mid) {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    lo
}

/// Where one byte anchor at `o` goes through `edits` — the store's rule for a single position, for a
/// caller holding one outside the store. `O(log E)` plus the edits touching `o`.
#[must_use]
pub fn map_byte(edits: &EditList, o: usize, bias: Bias) -> usize {
    map_byte_inside(edits.edits(), o, bias).0
}

/// A position a [`crate::Document`] carries through its changes for an owner that keeps it outside the
/// store (a View's marks, #686): `pos` is updated in place by each change, `None` once a
/// [`LineGravity::Drop`] line was deleted (or before it was ever set). `restore` = an undo puts it back
/// where it was when the undone change began, as Vim's `u_undoredo` restores the lowercase and Visual
/// marks.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Tracked {
    pub pos: Option<usize>,
    pub track: Track,
    pub restore: bool,
}

impl Tracked {
    /// A position followed by line (a Vim mark) that undo does not restore.
    #[must_use]
    pub fn line(pos: Option<usize>, gravity: LineGravity) -> Tracked {
        Tracked {
            pos,
            track: Track::Line(gravity),
            restore: false,
        }
    }

    /// Mark this position as one undo restores.
    #[must_use]
    pub fn restored(mut self) -> Tracked {
        self.restore = true;
        self
    }

    /// Carry this position through one change.
    pub fn apply(&mut self, edits: &EditList, map: &LineMap<'_>) {
        self.pos = self.pos.and_then(|p| match self.track {
            Track::Byte(bias) => Some(crate::motion::snap(map.new, map_byte(edits, p, bias))),
            Track::Line(g) => map.map(p, g),
        });
    }
}

/// The line-anchor rule (anchor-store §3.1): where a position on a line goes when one transaction's
/// edits apply, as nvim's `mark_adjust` moves a mark. A position is `(line, column)`:
///
/// - an edit inside its line, or one that starts in its line and splits it or joins the next line on,
///   leaves the column alone (clamped into the line);
/// - whole lines inserted or deleted before it shift its line; a deleted line takes the position with it
///   ([`LineGravity`]); a replaced block keeps it on the line of the same index while one is left;
/// - an edit that runs into its line from above (a join, a charwise delete across lines) moves it with
///   the text after the edit, or to the edit's end if it sat in the removed head.
///
/// A whole-line edit is one whose span starts and ends at line starts (`dd`, `O`, `:put`), or one that
/// starts at a line's `\n` and ends at one (`dd` of the last line, `o`): with no other clue, the first
/// reading wins. Cost per position is `O(log E)` plus the length of its line.
pub struct LineMap<'a> {
    pub(crate) old: &'a [u8],
    pub(crate) new: &'a [u8],
    edits: &'a [Edit],
    /// `prefix[i]` = the summed delta of `edits[..i]`.
    prefix: Vec<isize>,
}

impl<'a> LineMap<'a> {
    /// The map for `edits` (disjoint, sorted) turning `old` into `new`.
    #[must_use]
    pub fn new(old: &'a [u8], new: &'a [u8], edits: &'a EditList) -> LineMap<'a> {
        let edits = edits.edits();
        let mut prefix = Vec::with_capacity(edits.len() + 1);
        let mut sum = 0isize;
        prefix.push(0);
        for e in edits {
            sum += e.delta();
            prefix.push(sum);
        }
        LineMap {
            old,
            new,
            edits,
            prefix,
        }
    }

    fn line_start_at(&self, p: usize) -> bool {
        p == 0 || self.old.get(p - 1) == Some(&b'\n')
    }

    /// The edit's span starts and ends at line starts and it inserts whole lines (`dd`, `O`).
    fn start_aligned(&self, e: &Edit) -> bool {
        self.line_start_at(e.pos.0)
            && self.line_start_at(e.end())
            && (e.ins.is_empty() || e.ins.ends_with(b"\n"))
    }

    /// The edit's span starts at a line's `\n` and ends at one or the buffer end, and it inserts
    /// newline-led lines (`dd` of the last line, `o`).
    fn end_aligned(&self, e: &Edit) -> bool {
        self.old.get(e.pos.0) == Some(&b'\n')
            && (e.end() == self.old.len() || self.old.get(e.end()) == Some(&b'\n'))
            && (e.ins.is_empty() || e.ins.starts_with(b"\n"))
    }

    /// The new-text offset of column `col` on the line starting at `ls` (clamped onto the line).
    fn at_col(&self, ls: usize, col: usize) -> usize {
        let ls = ls.min(self.new.len());
        let le = crate::pos::line_end(self.new, ls);
        ls + col.min(le - ls)
    }

    /// The start of the new-text line holding `p`; the slot after a final newline is no line, so it
    /// reads as the last line.
    fn new_line_start(&self, p: usize) -> usize {
        let mut p = p.min(self.new.len());
        if p == self.new.len() && p > 0 && self.new[p - 1] == b'\n' {
            p -= 1;
        }
        crate::pos::line_start(self.new, p)
    }

    /// Where the position at `o` (an old-text offset) goes, or `None` when its line was deleted and
    /// `gravity` is [`LineGravity::Drop`]. Always a char boundary of the new text.
    #[must_use]
    pub fn map(&self, o: usize, gravity: LineGravity) -> Option<usize> {
        let o = o.min(self.old.len());
        let ls = crate::pos::line_start(self.old, o);
        let col = o - ls;
        let mut i = self.edits.partition_point(|e| e.end() < ls);
        let mut delta = self.prefix[i];
        let mut out = None;
        while let Some(e) = self.edits.get(i) {
            if e.pos.0 > ls {
                break;
            }
            let base = (e.pos.0 as isize + delta) as usize;
            if e.end() == ls && e.pos.0 < ls && (self.start_aligned(e) || !self.end_aligned(e)) {
                // Ends at this line's start: lines above it went or a join reached up to it.
                delta += e.delta();
                i += 1;
                continue;
            }
            if e.del == 0 && e.pos.0 == ls && e.ins.ends_with(b"\n") {
                // Whole lines inserted above this one.
                delta += e.delta();
                i += 1;
                continue;
            }
            if self.start_aligned(e) && ls < e.end() {
                // This line is one of a replaced block of whole lines.
                let j = crate::pos::count_newlines(&self.old[e.pos.0..ls]);
                out = Some(self.replaced(e, base, j, col, gravity, false));
            } else if e.pos.0 < ls && self.end_aligned(e) {
                // This line is one of the lines after the edit's `\n` it replaces.
                let j = crate::pos::count_newlines(&self.old[e.pos.0 + 1..ls]);
                out = Some(self.replaced(e, base, j, col, gravity, true));
            } else if e.pos.0 < ls {
                // A charwise edit runs into this line from above.
                let le = crate::pos::line_end(self.old, ls);
                out = Some(if e.end() > le {
                    match gravity {
                        LineGravity::Drop => None,
                        LineGravity::Keep => Some(base + e.ins.len()),
                        LineGravity::Cursor => Some(crate::pos::line_start(self.new, base)),
                    }
                } else if o >= e.end() {
                    Some((o as isize + delta + e.delta()) as usize)
                } else {
                    Some(base + e.ins.len())
                });
            }
            break;
        }
        let new = match out {
            Some(r) => r?,
            None => self.at_col((ls as isize + delta) as usize, col),
        };
        Some(crate::motion::snap(self.new, new))
    }

    /// Line `j` of the whole-line block `e` replaces (at `base` in the new text): the line of the same
    /// index in the inserted text while there is one, else `gravity`. `lead` = the inserted lines each
    /// follow a `\n` (an end-aligned edit) rather than end with one.
    fn replaced(
        &self,
        e: &Edit,
        base: usize,
        j: usize,
        col: usize,
        gravity: LineGravity,
        lead: bool,
    ) -> Option<usize> {
        // The start offsets (in `e.ins`) of the inserted lines.
        let mut starts: Vec<usize> = Vec::new();
        if !lead && !e.ins.is_empty() {
            starts.push(0);
        }
        for (k, &b) in e.ins.iter().enumerate() {
            if b == b'\n' && (lead || k + 1 < e.ins.len()) {
                starts.push(k + 1);
            }
        }
        if let Some(&s) = starts.get(j) {
            return Some(self.at_col(base + s, col));
        }
        match gravity {
            LineGravity::Drop => None,
            // The line after the block; past the buffer end, the last line.
            LineGravity::Keep => {
                let after = base + e.ins.len() + usize::from(lead && e.end() < self.old.len());
                Some(self.at_col(self.new_line_start(after), col))
            }
            // Column 0 of the line before the deleted ones: the block's last line, else the one above.
            LineGravity::Cursor => Some(match starts.last() {
                Some(&s) => base + s,
                None if lead => crate::pos::line_start(self.new, base),
                None if base == 0 => 0,
                None => crate::pos::line_start(self.new, base - 1),
            }),
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
        let (n, inside) = map_one(off, e.pos.0, e.del, e.ins.len(), bias);
        (n, inside && policy == AnchorPolicy::Invalidate)
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
                // Advance past the interval, often with no gap at all: a delete ending where the next edit
                // starts is the touching case the sweep must compose (#758).
                cursor += del + (rng() % 3) + usize::from(del == 0);
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

    #[test]
    fn touching_edits_compose_like_sequential() {
        // #758: `[del 2..4, ins@4]` and `[replace 2..4, ins@4]`, anchor at 4 with After bias.
        for (edits, want) in [
            (vec![Edit::delete(2, 2), Edit::insert(4, b"Z".to_vec())], 3),
            (
                vec![
                    Edit::replace(2, 2, b"Q".to_vec()),
                    Edit::insert(4, b"Z".to_vec()),
                ],
                4,
            ),
        ] {
            let list = EditList::new(edits).unwrap();
            let mut store = AnchorStore::new();
            let id = store.insert(4, Bias::After, AnchorPolicy::Clamp);
            store.apply_edits(&list);
            let (seq, _) = sequential(4, Bias::After, AnchorPolicy::Clamp, list.edits());
            assert_eq!(at(&store, id), seq);
            assert_eq!(at(&store, id), want);
        }
    }

    #[test]
    fn a_replacement_keeps_each_bias_in_order() {
        // #941: replace [2,6) with 3 bytes. Inside anchors take their bias at the insertion, as nvim moves
        // an extmark (delete, then insert), so no same-bias pair inverts.
        let list = EditList::new(vec![Edit::replace(2, 4, b"XYZ".to_vec())]).unwrap();
        let mut store = AnchorStore::new();
        let ids: Vec<(usize, Bias, AnchorId)> = [1usize, 2, 3, 5, 6, 7]
            .iter()
            .flat_map(|&o| [Bias::Before, Bias::After].map(|b| (o, b)))
            .map(|(o, b)| (o, b, store.insert(o, b, AnchorPolicy::Clamp)))
            .collect();
        store.apply_edits(&list);
        // nvim 0.12.6 `nvim_buf_set_text(0, 0, 2, 0, 6, {"XYZ"})` over extmarks at these columns:
        // (column, Before lands, After lands).
        let want = [
            (1, 1, 1),
            (2, 2, 5),
            (3, 2, 5),
            (5, 2, 5),
            (6, 2, 5),
            (7, 6, 6),
        ];
        for (o, before, after) in want {
            let find = |b| {
                let &(_, _, id) = ids.iter().find(|g| g.0 == o && g.1 == b).unwrap();
                at(&store, id)
            };
            assert_eq!(find(Bias::Before), before, "{o} Before");
            assert_eq!(find(Bias::After), after, "{o} After");
        }
    }

    // --- line anchors (anchor-store §3.1, nvim `mark_adjust`) ---

    /// Apply `edits` to `old` and map `o` with `g`.
    fn line(old: &str, edits: Vec<Edit>, o: usize, g: LineGravity) -> Option<usize> {
        let list = EditList::new(edits).unwrap();
        let new = list.apply_to(old.as_bytes());
        LineMap::new(old.as_bytes(), &new, &list).map(o, g)
    }

    #[test]
    fn a_line_anchor_keeps_its_column_through_an_edit_in_its_line() {
        // nvim: `5|ma0x`a` lands on col 4 again (the mark did not move left).
        assert_eq!(
            line("abcdefgh\n", vec![Edit::delete(0, 1)], 4, LineGravity::Drop),
            Some(4)
        );
        // `0iZZ<Esc>`: the column stays 4 too.
        let ins = vec![Edit::insert(0, b"ZZ".to_vec())];
        assert_eq!(line("abcdefgh\n", ins, 4, LineGravity::Drop), Some(4));
        // A column past the shortened line clamps onto its end.
        let del = vec![Edit::delete(0, 5)];
        assert_eq!(line("abcdefgh\nx\n", del, 7, LineGravity::Drop), Some(3));
    }

    #[test]
    fn a_line_anchor_follows_whole_lines_inserted_or_deleted_above() {
        // `O` above line 2: "x\n" at 2 moves the mark on `b` (2) to 4.
        let o = vec![Edit::insert(2, b"x\n".to_vec())];
        assert_eq!(line("a\nb\n", o, 2, LineGravity::Drop), Some(4));
        // `dd` of line 1 moves the mark on line 3 up a line, column kept.
        let dd = vec![Edit::delete(0, 2)];
        assert_eq!(line("a\nb\ncde\n", dd, 6, LineGravity::Drop), Some(4));
        // An edit after the line leaves it.
        let after = vec![Edit::delete(4, 2)];
        assert_eq!(line("a\nb\ncde\n", after, 2, LineGravity::Drop), Some(2));
    }

    #[test]
    fn a_deleted_line_drops_keeps_or_moves_the_cursor() {
        let text = "a\nbcd\ne\n";
        let dd = || vec![Edit::delete(2, 4)];
        assert_eq!(line(text, dd(), 4, LineGravity::Drop), None);
        // Keep: the line that took its place, column kept, clamped onto the line's end (`e\n`: 3).
        assert_eq!(line(text, dd(), 4, LineGravity::Keep), Some(3));
        // Cursor: column 0 of the line above.
        assert_eq!(line(text, dd(), 4, LineGravity::Cursor), Some(0));
        // The last line deleted through its leading `\n` ("a\nb" minus "\nb"): Keep lands on the new last
        // line, Cursor on column 0 of it.
        let last = || vec![Edit::delete(1, 2)];
        assert_eq!(line("a\nb", last(), 2, LineGravity::Drop), None);
        assert_eq!(line("a\nb", last(), 2, LineGravity::Keep), Some(0));
        assert_eq!(line("a\nb", last(), 2, LineGravity::Cursor), Some(0));
    }

    #[test]
    fn a_replaced_block_keeps_marks_on_the_lines_left() {
        // `:2,3!` filtering two lines into one: line 2's mark stays, line 3's is dropped.
        let f = || vec![Edit::replace(2, 6, b"XY\n".to_vec())];
        let text = "a\nbbb\nc\nd\n";
        assert_eq!(line(text, f(), 3, LineGravity::Drop), Some(3));
        assert_eq!(line(text, f(), 6, LineGravity::Drop), None);
        assert_eq!(line(text, f(), 6, LineGravity::Keep), Some(5));
        assert_eq!(line(text, f(), 8, LineGravity::Drop), Some(5));
    }

    #[test]
    fn a_join_moves_the_next_lines_marks_onto_the_joined_line() {
        // `J` on "aa\n  bbbb": "\n  " becomes " ". nvim moves the mark at col 2 (`b`) to col 3.
        let j = || vec![Edit::replace(2, 3, b" ".to_vec())];
        assert_eq!(line("aa\n  bbbb", j(), 5, LineGravity::Drop), Some(3));
        // In the removed indent: onto the first joined char.
        assert_eq!(line("aa\n  bbbb", j(), 4, LineGravity::Drop), Some(3));
        // The first line's marks stay.
        assert_eq!(line("aa\n  bbbb", j(), 1, LineGravity::Drop), Some(1));
    }

    #[test]
    fn opening_a_line_below_leaves_the_line_above() {
        // `o` as "\nx" at the end of an EMPTY line: the mark on it stays (an end-aligned insert).
        let o = vec![Edit::insert(2, b"\nx".to_vec())];
        assert_eq!(line("a\n\nb\n", o, 2, LineGravity::Drop), Some(2));
        // Splitting a line keeps its marks on the first half, clamped (nvim `i<CR>`).
        let split = vec![Edit::insert(2, b"\n".to_vec())];
        assert_eq!(line("abcdefgh\n", split, 5, LineGravity::Drop), Some(2));
    }

    #[test]
    fn line_anchors_in_the_store() {
        let old = b"a\nb\nc\n".to_vec();
        let list = EditList::new(vec![Edit::delete(2, 2)]).unwrap();
        let new = list.apply_to(&old);
        let mut store = AnchorStore::new();
        let drop = store.insert_line(2, LineGravity::Drop);
        let keep = store.insert_line(2, LineGravity::Keep);
        let below = store.insert_line(4, LineGravity::Drop);
        store.apply_change(&old, &new, &list);
        assert!(store.resolve(drop).invalidated);
        assert_eq!(store.resolve(keep).offset.0, 2);
        assert!(!store.resolve(keep).invalidated);
        assert_eq!(store.resolve(below).offset.0, 2);
    }
}
