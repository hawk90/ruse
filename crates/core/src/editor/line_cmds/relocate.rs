//! `:m` (move) and `:t` / `:copy` (copy): one engine ([`EditorState::relocate_lines`]) that edits only
//! the block it moves or copies, and the whole-text line-model rewrite ([`relocated_by_lines`]) for the
//! one shape that does not split that way.

use crate::edit::{Edit, EditList};
use crate::editor::{line_count, line_span, resolve_lines, EditorState, LineAddr, SubRange};
use crate::pos::BytePos;

impl EditorState {
    /// `:[range]m {addr}` — move the range's lines to after the destination line, as one undo group.
    /// Returns the number of lines moved, or `None` if the destination lies inside the source (Vim's
    /// "move lines into themselves"). No range = the cursor's line.
    pub fn move_lines(&mut self, range: SubRange, dest: LineAddr) -> Option<usize> {
        self.relocate_lines(range, dest, false)
    }

    /// `:[range]t {addr}` / `:copy` — COPY the range's lines to after the destination line (the source
    /// stays), as one undo group. Returns the number of lines copied, or `None` for a relative destination
    /// off the buffer.
    pub fn copy_lines(&mut self, range: SubRange, dest: LineAddr) -> Option<usize> {
        self.relocate_lines(range, dest, true)
    }

    /// Shared engine for `:m` (move) and `:t`/`:copy` (`copy = true` keeps the source), as one
    /// transaction over the lines it moves or copies (#1257): its lines come from the document's line
    /// index and only the block is copied, so a `:g` that runs it per line does not rescan the buffer.
    fn relocate_lines(&mut self, range: SubRange, dest: LineAddr, copy: bool) -> Option<usize> {
        let old = self.doc.bytes();
        if old.is_empty() {
            return Some(0);
        }
        let nlines = line_count(&self.doc);
        let cursor_line = self.doc.line_of(BytePos(self.view.cursor));
        let (first, last) = resolve_lines(range, nlines, cursor_line);
        // Destination as a 0-based insert index in `0..=nlines`; a relative one off the buffer declines.
        let ins = dest.insert_index(cursor_line, nlines)?;
        // A move INTO the block is E134 (declined); a move to just before or just after it changes nothing
        // but still puts the cursor on the block's last line, as Vim's `do_move` does (`:2m2`, #724).
        if !copy && ins > first && ins <= last {
            return None;
        }
        // The block is `old[s..e]` (its lines with their newlines; an unterminated last line has none),
        // and `p` is where the destination line ends: the start of the line after it.
        let s = line_span(&self.doc, first).0;
        let count = last - first + 1;
        if !copy && (ins == first || ins == last + 1) {
            let ls = self.doc.nth_line_start(last).0;
            self.view.cursor = self.relocated_cursor(old, ls);
            return Some(0);
        }
        let e = self.doc.nth_line_start(last + 1).0;
        let p = self.doc.nth_line_start(ins).0;
        // After a move removes the block, an insert index past the block shifts left by `count`.
        let adj = if copy || ins <= first {
            ins
        } else {
            ins - count
        };
        let edits = block_edits(old, (s, e), p, copy).unwrap_or_else(|| {
            vec![Edit::lines_changed(
                old,
                relocated_by_lines(old, first, last, ins, copy),
            )]
        });
        let list = EditList::new(edits).expect("the move's delete and insert are disjoint");
        // `u` and `CTRL-R` start from where the cursor stood: Vim's `u_save` in `ex_copy`/`do_move` runs
        // before the cursor moves, so `uh_cursor` is the cursor before the command (#1258).
        self.apply_ex_edit(list, self.origin, Some(self.view.cursor.min(old.len())))
            .expect("line relocate applies cleanly");
        self.view.last_was_edit = true;
        // On the block's last line where it now is.
        let new = self.doc.bytes();
        let ls = self.doc.nth_line_start(adj + count - 1).0;
        self.view.cursor = self.relocated_cursor(new, ls).min(new.len());
        self.settle_offsets();
        Some(count)
    }
}

/// The edits of [`EditorState::relocate_lines`] for the block `old[s..e]` going to `p` (the start of the
/// line after the destination), or `None` for the one shape they cannot split.
///
/// A move is the block deleted at its place and inserted at the destination, so the line anchors on
/// it go along (nvim's `ex_copymove` moves the marks, #1125). Without a final newline the last line
/// has none: a block holding it is cut with the `\n` before it, and one moved after it goes in after
/// a `\n`. The one shape that does not split that way (the last line moved onto an empty line just
/// above it) takes the whole-region rewrite, whose marks keep their lines. A copy is the block
/// inserted, recorded as the one line edit [`Edit::lines_changed`] finds for it.
fn block_edits(old: &[u8], (s, e): (usize, usize), p: usize, copy: bool) -> Option<Vec<Edit>> {
    let had_trailing_nl = old.ends_with(b"\n");
    let mut block = old[s..e].to_vec();
    if copy {
        if !had_trailing_nl {
            if p == old.len() {
                // After the unterminated last line: `\n` + the block without its own.
                if e < old.len() {
                    block.pop();
                }
                block.insert(0, b'\n');
            } else if e == old.len() {
                block.push(b'\n'); // the unterminated last line copied above: give it its newline
            }
        }
        Some(vec![Edit::lines_changed_by_insert(old, p, block)])
    } else if had_trailing_nl || (e < old.len() && p < old.len()) {
        Some(if p < s {
            vec![
                Edit::insert(BytePos(p), block),
                Edit::delete(BytePos(s), e - s),
            ]
        } else {
            vec![
                Edit::delete(BytePos(s), e - s),
                Edit::insert(BytePos(p), block),
            ]
        })
    } else if e < old.len() {
        // After the unterminated last line: `\n` + the block without its own.
        block.pop();
        block.insert(0, b'\n');
        Some(vec![
            Edit::delete(BytePos(s), e - s),
            Edit::insert(BytePos(p), block),
        ])
    } else if p + 1 < s {
        // The unterminated last line moves up: cut it with the `\n` before it.
        block.push(b'\n');
        Some(vec![
            Edit::insert(BytePos(p), block),
            Edit::delete(BytePos(s - 1), e - s + 1),
        ])
    } else {
        None
    }
}

/// The text `:m` (or `:t`, with `copy`) makes of the non-empty `old`, by the line model: its lines with
/// `first..=last` (0-based, in range) moved or copied to before line `ins` (`0..=lines`), the final
/// newline kept as it was. [`EditorState::relocate_lines`] edits only the block; this is the rewrite
/// for the one shape it cannot split, and the reference its tests hold it to.
pub(in crate::editor) fn relocated_by_lines(
    old: &[u8],
    first: usize,
    last: usize,
    ins: usize,
    copy: bool,
) -> Vec<u8> {
    let had_trailing_nl = old.ends_with(b"\n");
    let mut lines: Vec<&[u8]> = old.split(|&c| c == b'\n').collect();
    if had_trailing_nl {
        lines.pop(); // drop the empty element after the final '\n'
    }
    let block: Vec<&[u8]> = lines[first..=last].to_vec();
    let count = block.len();
    if !copy {
        lines.drain(first..=last);
    }
    let adj = if copy || ins <= first {
        ins
    } else {
        ins - count
    }
    .min(lines.len());
    lines.splice(adj..adj, block);
    let mut new = lines.join(&b'\n');
    if had_trailing_nl && !new.is_empty() {
        new.push(b'\n');
    }
    new
}
