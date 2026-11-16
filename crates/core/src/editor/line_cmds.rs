//! The Ex line commands as `EditorState` methods. Moved out of `editor/mod.rs` (#1352); a child module,
//! so they read the `View` and `Document` as before. This file holds the core range commands — `:d`,
//! `:y`, `:j`, `:>`/`:<` — and what the families share; each other family is a child (#1499):
//!
//! - `global` — `:g` / `:g!` / `:v` (the marking pass and the `d` / `s///` payloads),
//! - `relocate` — `:m` and `:t` / `:copy`,
//! - `insert` — `:put`, `:r`, the `:{range}!` filter and the text it reads,
//! - `retab` — `:retab`,
//! - `sort` — `:sort` and its key readers,
//! - `batch` — the LSP/completion edit batch (`apply_edits`).

use super::*;

mod batch;
mod global;
mod insert;
mod relocate;
mod retab;
mod sort;

#[cfg(test)]
pub(super) use relocate::relocated_by_lines;

/// The buffer's lines, each WITHOUT its trailing newline (no element for the slot after a final newline;
/// none at all for an empty buffer): how `:put`, `:r`, the filter and `:sort` see the text they rewrite.
fn buffer_lines(buf: &[u8]) -> Vec<&[u8]> {
    if buf.is_empty() {
        Vec::new()
    } else {
        let mut v: Vec<&[u8]> = buf.split(|&b| b == b'\n').collect();
        if buf.ends_with(b"\n") {
            v.pop(); // drop the empty element after the final '\n'
        }
        v
    }
}

/// `lines` joined back into a buffer, ending in a newline when `had_trailing_nl` — one empty line is still
/// a line: `\n`, not the emptied buffer (RFC-0024). `capacity` is the expected size.
fn join_buffer_lines(lines: &[&[u8]], had_trailing_nl: bool, capacity: usize) -> Vec<u8> {
    let mut new_bytes: Vec<u8> = Vec::with_capacity(capacity);
    for (i, line) in lines.iter().enumerate() {
        if i > 0 {
            new_bytes.push(b'\n');
        }
        new_bytes.extend_from_slice(line);
    }
    if had_trailing_nl && !lines.is_empty() {
        new_bytes.push(b'\n');
    }
    new_bytes
}

/// The byte offset where 0-based line `idx` of `bytes` starts: the lengths of the `idx` lines before it
/// (with their newlines). Past the last line it is the end of the text.
fn line_start_of(bytes: &[u8], idx: usize) -> usize {
    bytes
        .split_inclusive(|&b| b == b'\n')
        .take(idx)
        .map(<[u8]>::len)
        .sum()
}

impl EditorState {
    /// `:[range]d` — delete the lines in `range` (each with its trailing newline) as one undo group,
    /// like a linewise `dd` over the span. No range = the cursor's line. Returns the number of lines
    /// deleted (0 on an empty buffer). The cursor lands at the start of the line that took their place.
    pub fn delete_lines(&mut self, range: SubRange) -> usize {
        self.delete_lines_into(range, None)
    }

    /// `:[range]d {x}` — [`EditorState::delete_lines`] into register `reg` (`None` = the unnamed one).
    pub fn delete_lines_into(&mut self, range: SubRange, reg: Option<char>) -> usize {
        let bytes = self.doc.bytes();
        let hay: &[u8] = bytes;
        let cursor_line = self.doc.line_of(BytePos(self.view.cursor));
        let (first, last) = resolve_lines(range, line_count(&self.doc), cursor_line);
        let (fs, first_end) = line_span(&self.doc, first);
        let (_, le) = line_span(&self.doc, last);
        // Terminated last line: delete `[fs, le+1)` (the lines plus their trailing newlines). Unterminated
        // last line (deleting through EOF): delete `[fs-1, le)` to absorb the newline BEFORE the span so no
        // dangling blank line remains — except when `first` is line 0 (whole buffer), then `[0, le)`.
        let (start, end) = if le < hay.len() {
            (fs, le + 1)
        } else if fs > 0 {
            (fs - 1, le)
        } else {
            (0, le)
        };
        if end == start {
            return 0;
        }
        // The deleted lines fill the unnamed register (and shift the numbered ring) linewise, as `dd` does
        // (`:help :d`); `Register::linewise` terminates an unterminated last line.
        let lines_text = bytes[fs..le].to_vec();
        self.view
            .registers
            .delete(reg, Register::linewise(lines_text));
        let list = EditList::new(vec![Edit::delete(BytePos(start), end - start)])
            .expect("a single line-range delete is well-formed");
        // Vim moves the cursor to the range's first line (keeping its column) before deleting, so that is
        // where `u` puts it back (nvim 0.12.6: `4l:3d<CR>u` -> line 3, column 4).
        let col = self.view.cursor - line_start(bytes, self.view.cursor);
        let undo_at = snap(bytes, (fs + col).min(first_end));
        self.apply_ex_edit(list, self.origin, Some(undo_at))
            .expect("line-range delete applies cleanly");
        self.view.last_was_edit = true;
        // On the line that took their place, or the new last line (never the slot after a final newline,
        // nor past the end of an unterminated last line), at its first non-blank.
        let nb = self.doc.bytes();
        let at = motion::off_phantom_at(nb, start.min(nb.len()));
        self.view.cursor = motion::first_non_blank(nb, line_start(nb, at));
        self.settle_offsets();
        self.view.curswant = self.wanted_col(); // Vim's `beginline` resets the wanted column (#1104)
        last - first + 1
    }

    /// `:[range]y` — yank the range's lines LINEWISE into the unnamed register (and `"0`), like `yy` over
    /// the span (no range = the cursor's line). Returns the number of lines yanked. Non-destructive: the
    /// buffer and cursor are unchanged (Vim moves the cursor to the range's last line; deferred).
    pub fn yank_lines(&mut self, range: SubRange) -> usize {
        self.yank_lines_into(range, None)
    }

    /// `:[range]y {x}` — [`EditorState::yank_lines`] into register `reg` (`None` = the unnamed one).
    pub fn yank_lines_into(&mut self, range: SubRange, reg: Option<char>) -> usize {
        let bytes = self.doc.bytes();
        let hay: &[u8] = bytes;
        let cursor_line = self.doc.line_of(BytePos(self.view.cursor));
        let (first, last) = resolve_lines(range, line_count(&self.doc), cursor_line);
        let (fs, _) = line_span(&self.doc, first);
        let (_, le) = line_span(&self.doc, last);
        let end = if le < hay.len() { le + 1 } else { le };
        // `Register::linewise` normalizes to end with `\n`, so an unterminated last line still yanks clean.
        let text = bytes[fs..end].to_vec();
        self.view.registers.yank(reg, Register::linewise(text));
        last - first + 1
    }

    /// `:[range]j[oin][!]` — join the range's lines into one, reusing the SAME core join the normal-mode
    /// `J`/`gJ` emit (`Command::JoinLines`/`JoinLinesNoSpace`) so the whitespace rules stay identical: a
    /// single separating space, suppressed before `)`, after a line already ending in whitespace, or on an
    /// empty line; the next line's leading whitespace is stripped. `no_space` (the `!` bang) is the `gJ`
    /// raw-concatenate form. No range = the cursor's line; like Vim, a single-line target joins it with the
    /// NEXT line. The cursor lands on the join seam, exactly where `J` leaves it. Returns the number of
    /// lines that were joined (0 if there was nothing to join).
    pub fn join_lines(&mut self, range: SubRange, no_space: bool) -> usize {
        let cursor_line = self.doc.line_of(BytePos(self.view.cursor));
        let (first, last) = resolve_lines(range, line_count(&self.doc), cursor_line);
        let first_start = line_span(&self.doc, first).0;
        // Position the cursor at the range's first line so the reused join starts there; the count is the
        // line span, which `plan_join` clamps to `.max(2)` — so a single-line range joins the current line
        // with the next, matching Vim.
        self.view.cursor = first_start;
        let count = (last - first + 1) as u32;
        let cmd = if no_space {
            Command::JoinLinesNoSpace(count)
        } else {
            Command::JoinLines(count)
        };
        let before = self.doc.line_starts().len();
        apply_command(self, &cmd);
        // Unlike `J` (cursor on the join seam), Vim's `:join` leaves the cursor on the FIRST NON-BLANK of the
        // joined line (`ex_join` → `beginline(BL_WHITE)`; nvim v0.12.6). The join keeps `first`'s start.
        let b = self.doc.bytes();
        self.view.cursor = motion::first_non_blank(b, first_start);
        self.view.curswant = self.wanted_col(); // Vim's `beginline` resets the wanted column (#1104)
                                                // The lines that took part: one more than the seams removed (0 when nothing joined).
        let seams = before - self.doc.line_starts().len();
        if seams == 0 {
            0
        } else {
            seams + 1
        }
    }

    /// `:[range]>` / `:[range]<` — shift the range's lines one indent level right (`left = false`) or left
    /// (`left = true`), reusing the SAME core shift the normal-mode `>>`/`<<` emit (`Command::ShiftRight`/
    /// `ShiftLeft`), so the indent unit, whitespace-only-vs-empty-line rule, and clamp-at-column-0 all stay
    /// identical. `levels` applies the shift that many times (Vim's repeated verb chars: `:>>>` = 3). All the
    /// levels merge into ONE undo group (the second and later applies see `last_was_edit`, so their group
    /// hint is `Continue`). Unlike normal `>>`, the cursor lands on the first non-blank of the range's LAST
    /// line (Vim's ex-shift behaviour — verified against nvim v0.12.4). No range = the cursor's line. Returns
    /// the number of lines in the range.
    pub fn shift_lines(&mut self, range: SubRange, left: bool, levels: u32) -> usize {
        let cursor_line = self.doc.line_of(BytePos(self.view.cursor));
        let (first, last) = resolve_lines(range, line_count(&self.doc), cursor_line);
        let count = (last - first + 1) as u32;
        let want = self.view.curswant.clone();
        // Anchor the cursor at the FIRST line so the reused shift (which reads its start line from the
        // cursor) covers `[first, last]`; every extra level re-shifts the same span from there.
        let first_start = line_span(&self.doc, first).0;
        self.view.cursor = first_start;
        let cmd = if left {
            Command::ShiftLeft(count)
        } else {
            Command::ShiftRight(count)
        };
        for _ in 0..levels.max(1) {
            apply_command(self, &cmd);
        }
        // Vim's `:>`/`:<` leaves the cursor on the first non-blank of the range's LAST line (normal `>>`
        // leaves it on the FIRST). A shift never adds/removes lines, so `last` still names that line;
        // recompute its start against the POST-shift buffer (indent changed the byte offsets).
        let b = self.doc.bytes();
        if last < line_count(&self.doc) {
            let last_ls = self.doc.nth_line_start(last).0;
            self.view.cursor = motion::first_non_blank(b, last_ls);
        }
        // `:>` keeps the wanted column (`beginline(BL_SOL | BL_FIX)` under 'nostartofline', #1104).
        self.view.curswant = want;
        count as usize
    }

    /// Where `:m`/`:t` leave the cursor on the line starting at `ls` of `bytes`: the wanted column (Vim
    /// `ex_copymove` → `beginline(BL_SOL | BL_FIX)`, which with 'nostartofline' keeps `curswant`; nvim
    /// v0.12.6), on a character of the line.
    fn relocated_cursor(&self, bytes: &[u8], ls: usize) -> usize {
        let le = line_end(bytes, ls);
        let cursor = if self.view.curswant == range::MAXCOL {
            motion::prev_grapheme(bytes, le).max(ls)
        } else {
            range::want_col(bytes, ls, self.view.curswant.get(), self.view.indent.tabs())
        };
        if cursor >= le && le > ls {
            motion::prev_grapheme(bytes, le)
        } else {
            cursor
        }
    }
}
