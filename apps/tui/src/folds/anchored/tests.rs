use super::*;
use crate::folds::tests::f;

fn doc(text: &str) -> Document {
    Document::new(ruse_core::DocumentId(1), text.as_bytes().to_vec())
}

/// Apply one edit at `pos` deleting `del` bytes and inserting `ins`.
fn edit(doc: &mut Document, pos: usize, del: usize, ins: &str) {
    let edits = ruse_core::EditList::new(vec![ruse_core::Edit::replace(
        ruse_core::BytePos(pos),
        del,
        ins.as_bytes().to_vec(),
    )])
    .unwrap();
    let txn = ruse_core::Transaction::new(
        doc.revision(),
        edits,
        ruse_core::TransactionOrigin::UserInput,
    );
    doc.apply(txn).unwrap();
}

fn line_starts(b: &[u8]) -> Vec<usize> {
    std::iter::once(0)
        .chain(
            b.iter()
                .enumerate()
                .filter(|(_, &c)| c == b'\n')
                .map(|(i, _)| i + 1),
        )
        .collect()
}

fn line_of(b: &[u8], o: usize) -> usize {
    b[..o.min(b.len())].iter().filter(|&&c| c == b'\n').count()
}

/// The folds after `change` edits `text` with `folds` anchored over it.
fn followed_from(
    text: &str,
    folds: &[Fold],
    change: impl FnOnce(&mut Document),
) -> Vec<(usize, usize)> {
    let mut d = doc(text);
    let mut folds = folds.to_vec();
    let mut a = Anchored::default();
    let ls = line_starts(d.bytes());
    a.anchor(&mut d, &folds, |l| ls[l]);
    change(&mut d);
    let b = d.bytes().to_vec();
    a.follow(&d, &mut folds, |o| line_of(&b, o));
    a.release(&mut d);
    assert_eq!(d.anchor_count(), 0, "release frees every anchor");
    folds.iter().map(|f| (f.start, f.end)).collect()
}

/// The folds after `change` edits six lines `L1`…`L6` with a fold over lines 1-3 (0-based).
fn followed(change: impl FnOnce(&mut Document)) -> Vec<(usize, usize)> {
    followed_from("L1\nL2\nL3\nL4\nL5\nL6\n", &[f(1, 3, true)], change)
}

/// Each expectation is nvim's `foldclosed()`/`foldclosedend()` after the same change (0-based).
#[test]
fn manual_folds_follow_edits_like_nvim() {
    // `o` on the fold's last line (`L4` + `\nNEW`): outside the fold.
    assert_eq!(followed(|d| edit(d, 11, 0, "\nNEW")), vec![(1, 3)]);
    // `O` on its first line: the fold moves down.
    assert_eq!(followed(|d| edit(d, 3, 0, "NEW\n")), vec![(2, 4)]);
    // A line put inside it grows it.
    assert_eq!(followed(|d| edit(d, 6, 0, "X\n")), vec![(1, 4)]);
    // `dd` of all its lines removes it; so does a delete running past it.
    assert_eq!(followed(|d| edit(d, 3, 9, "")), vec![]);
    assert_eq!(followed(|d| edit(d, 3, 12, "")), vec![]);
    // Deleting its first line or its last line shrinks it.
    assert_eq!(followed(|d| edit(d, 3, 3, "")), vec![(1, 2)]);
    assert_eq!(followed(|d| edit(d, 9, 3, "")), vec![(1, 2)]);
    // Deleting lines above shifts it up.
    assert_eq!(followed(|d| edit(d, 0, 3, "")), vec![(0, 2)]);
    // `cc` over the whole fold leaves a one-line fold (nvim `foldlevel` 1 on the new line).
    assert_eq!(followed(|d| edit(d, 3, 8, "!")), vec![(1, 1)]);
    // `J` on its first line joins two of its lines.
    assert_eq!(followed(|d| edit(d, 5, 1, " ")), vec![(1, 2)]);
    // `J` on the line above joins its first line out of it.
    assert_eq!(followed(|d| edit(d, 2, 1, " ")), vec![(1, 2)]);
}

/// Two folds that meet: `J` on the first one's last line joins the second's first line into it, which
/// leaves the second fold starting a line later (nvim), so the two stay apart.
#[test]
fn a_join_between_two_folds_keeps_them_apart() {
    let text = "L1\nL2\nL3\nL4\nL5\nL6\n";
    let folds = [f(1, 2, true), f(3, 4, true)];
    assert_eq!(
        followed_from(text, &folds, |d| edit(d, 8, 1, " ")),
        vec![(1, 2), (3, 3)]
    );
}

/// A fold at the top whose lines are all deleted is gone too (its `Cursor` anchor stays on line 0).
#[test]
fn a_deleted_fold_at_the_top_is_gone() {
    assert_eq!(
        followed_from("L1\nL2\nL3\nL4\n", &[f(0, 1, true)], |d| edit(d, 0, 6, "")),
        vec![]
    );
    // Its first lines deleted, the rest of it stays.
    assert_eq!(
        followed_from("L1\nL2\nL3\nL4\n", &[f(0, 2, true)], |d| edit(d, 0, 6, "")),
        vec![(0, 0)]
    );
}
