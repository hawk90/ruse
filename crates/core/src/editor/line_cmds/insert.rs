//! The line commands that splice whole lines of outside text into the buffer: `:put` (a register),
//! `:r` (a file or a command's stdout) and the `:{range}!` filter, plus [`EditorState::range_text`],
//! which feeds that filter's stdin.

use super::{buffer_lines, join_buffer_lines, line_start_of};
use crate::edit::{Edit, EditList};
use crate::editor::{line_spans, resolve_line_range, EditorState, LineAddr, SubRange};
use crate::motion::{self, line_end, line_start};

impl EditorState {
    /// `:[line]put [reg]` — put a register's text LINEWISE as new whole line(s) after the addressed line,
    /// as one undo group. UNLIKE normal-mode `p`, a put is ALWAYS linewise (Vim `:help :put`): a charwise
    /// register's text is split on `\n` and each piece inserted as its own line — this is the
    /// linewise-FORCING rule (`:put` of a charwise `foo` opens a whole new line `foo`). The destination
    /// [`LineAddr`] is resolved as for `:m`/`:t`: `Line(0)` inserts at the very top (before line 1),
    /// `Line(n)` after 1-based line n, `Last` after the last line, and `Current` after the cursor's line
    /// (the bare `:put` default). The cursor lands on the first non-blank of the LAST inserted line
    /// (verified against nvim v0.12.4). `reg` is the register name (`None` = the unnamed register); reading
    /// it reuses the shared [`RegisterStore`](crate::register::RegisterStore), never a new store. Returns
    /// the number of lines inserted (0 — a no-op leaving the cursor put — if the register was never
    /// written). A register written with nothing (`qaq`, an expression that is `''`) puts one empty line,
    /// as nvim does (#1342).
    pub fn put_lines(&mut self, addr: LineAddr, reg: Option<char>) -> usize {
        // Reuse the shared register store: read the named slot's bytes (unnamed when `reg` is None). The
        // put is a command read of that text: split on '\n' the same way (a normalized LINEWISE register
        // ends with a '\n', which adds no empty line; a CHARWISE one with no trailing newline still splits
        // into whole lines — the linewise-forcing rule), the cursor on the LAST inserted line.
        let slot = self.view.registers.get(reg);
        let mut reg_text = slot.text().to_vec();
        if reg_text.is_empty() && slot.is_set() {
            reg_text.push(b'\n');
        }
        let count = self.read_lines(addr, &reg_text, true);
        // A put leaves `']` on the last char of its last line (nvim's `do_put`, #1303); the cursor is on
        // that line.
        if count > 0 {
            let b = self.doc.bytes();
            let (ls, le) = (
                line_start(b, self.view.cursor),
                line_end(b, self.view.cursor),
            );
            self.view.change_end = Some(if le > ls {
                motion::prev_grapheme(b, le)
            } else {
                ls
            });
        }
        count
    }

    /// `:[addr]r[ead] {file}` / `:[addr]r[ead] !{cmd}` — insert `text` (a file's contents or a command's
    /// stdout, read IMPURELY by the frontend — core stays IO-free) as new whole LINE(s) below the addressed
    /// line, as one undo group. `text` is split on `\n`; a trailing `\n` does NOT add a spurious empty line
    /// (a file with no final newline still inserts its last line). The destination [`LineAddr`] resolves as
    /// for `:m`/`:t`/`:put`: `Line(0)` inserts at the very top (before line 1, Vim's `:0r`), `Line(n)` after
    /// 1-based line n, `Last` after the last line, `Current` after the cursor's line (the bare `:r` default).
    /// The cursor lands on the FIRST non-blank of the FIRST inserted line for a file read (`cursor_on_last =
    /// false`), and of the LAST inserted line for a command read (`cursor_on_last = true`) — a real nvim
    /// v0.12.4 quirk (`:r file` and `:r !cmd` differ), reproduced faithfully. Returns the number of lines
    /// inserted (0 — a no-op leaving the cursor put — for empty `text`).
    pub fn read_lines(&mut self, addr: LineAddr, text: &[u8], cursor_on_last: bool) -> usize {
        if text.is_empty() {
            return 0;
        }
        // Into the emptied buffer the lines go around its one line (nvim: `:r` after it, `:0r` before it).
        if self.makes_the_emptied_line() {
            return self.on_the_emptied_line(|st| st.read_lines(addr, text, cursor_on_last));
        }
        let buf = self.doc.bytes();
        let hay: &[u8] = buf;
        let orig_len = buf.len();
        let had_trailing_nl = hay.ends_with(b"\n");
        // The lines to insert: split on '\n', dropping the empty element after a final newline so a file that
        // ends in '\n' does not read a spurious blank line (a file with NO final newline keeps its last line).
        let mut ins_lines: Vec<&[u8]> = text.split(|&b| b == b'\n').collect();
        if text.last() == Some(&b'\n') {
            ins_lines.pop();
        }
        if ins_lines.is_empty() {
            return 0;
        }
        // The buffer's existing lines (each WITHOUT its trailing newline).
        let mut buf_lines = buffer_lines(buf);
        let nlines = buf_lines.len();
        // Destination as a 0-based insert index in `0..=nlines` (Vim `:r N` = "after line N", `:0r` = top;
        // a bare `:r` = after the cursor's line).
        let cursor_line = crate::pos::raw::line_of(buf, self.view.cursor);
        let Some(ins) = addr.insert_index(cursor_line, nlines) else {
            return 0; // a relative address off the buffer inserts nothing
        };
        let count = ins_lines.len();
        for (k, line) in ins_lines.into_iter().enumerate() {
            buf_lines.insert(ins + k, line);
        }
        let new_bytes = join_buffer_lines(&buf_lines, had_trailing_nl, orig_len + text.len() + 1);
        // Cursor target line: the FIRST inserted line for a file read, the LAST for a command read; its
        // line start is computed now, before `new_bytes` is moved into the edit.
        let target_idx = if cursor_on_last { ins + count - 1 } else { ins };
        let line_start = line_start_of(&new_bytes, target_idx);
        let list = EditList::new(vec![Edit::lines_changed(self.doc.bytes(), new_bytes)])
            .expect("a single edit is well-formed");
        let group = self.doc.history().current_group();
        self.apply_ex_edit(list, self.origin, None)
            .expect("line read applies cleanly");
        self.view.last_was_edit = true;
        // `'[` and `'.` at the first line read, `']` at the last (nvim's `readfile`, #1303).
        let start = self.line_pos(ins);
        self.set_ex_marks((start, self.line_pos(ins + count - 1)), start, Some(group));
        let nb = self.doc.bytes();
        self.view.cursor = motion::first_non_blank(nb, line_start.min(nb.len()));
        self.settle_offsets();
        self.view.curswant = self.wanted_col(); // Vim's `beginline` resets the wanted column (#1104)
        count
    }

    /// The text of the lines in `range` (each with a trailing `\n`), as Vim feeds a filter command's stdin
    /// for `:{range}!{cmd}`: the buffer's bytes as they are, whatever their encoding. `None` for an empty
    /// buffer. No range = the current line.
    #[must_use]
    pub fn range_text(&self, range: SubRange) -> Option<Vec<u8>> {
        let bytes = self.doc.bytes();
        if bytes.is_empty() {
            return None;
        }
        let spans = line_spans(bytes);
        let cursor_line = crate::pos::raw::line_of(bytes, self.view.cursor);
        let (first, last) = resolve_line_range(range, &spans, cursor_line);
        let mut out = Vec::new();
        for &(s, e) in &spans[first..=last] {
            out.extend_from_slice(&bytes[s..e]);
            out.push(b'\n');
        }
        Some(out)
    }

    /// `:{range}!{cmd}` — FILTER: replace the lines in `range` with `text` (the filter command's stdout, split
    /// on `\n` with the trailing-newline element dropped, exactly like [`EditorState::read_lines`]), as one
    /// undo group. An empty `text` deletes the range's lines (Vim: a filter that emits nothing). The cursor
    /// lands on the FIRST non-blank of the first line of the replaced region (verified vs nvim v0.12.4).
    /// Returns the number of INPUT lines filtered (0 for an empty buffer), matching Vim's "N lines
    /// filtered" report. Bytes are spliced as they are, so a buffer that is not UTF-8 filters too.
    pub fn filter_lines(&mut self, range: SubRange, text: &[u8]) -> usize {
        let buf = self.doc.bytes();
        let hay = buf;
        let orig_len = buf.len();
        let had_trailing_nl = hay.ends_with(b"\n");
        let mut lines = buffer_lines(buf);
        let nlines = lines.len();
        if nlines == 0 {
            return 0;
        }
        let spans = line_spans(hay);
        let cursor_line = crate::pos::raw::line_of(buf, self.view.cursor);
        let (first, last) = resolve_line_range(range, &spans, cursor_line);
        let last = last.min(nlines - 1);
        let first = first.min(last);
        let input_count = last - first + 1;
        // The replacement lines from the filter's stdout (same split rule as `read_lines`).
        let mut new_lines: Vec<&[u8]> = text.split(|&b| b == b'\n').collect();
        if text.last() == Some(&b'\n') {
            new_lines.pop();
        }
        if text.is_empty() {
            new_lines.clear();
        }
        // Splice: lines[..first] ++ new_lines ++ lines[last+1..].
        lines.splice(first..=last, new_lines.iter().copied());
        let new_bytes = join_buffer_lines(&lines, had_trailing_nl, orig_len + text.len() + 1);
        // Cursor: the first line of the replaced region (clamped when the filter shortened the buffer).
        let target_idx = first.min(lines.len().saturating_sub(1));
        let line_start = line_start_of(&new_bytes, target_idx);
        // Undo and redo come back to the start of the range's LAST line (#1165): nvim's `do_filter` saves
        // the change with the cursor there (`uh_cursor`), having moved it to `line2` to append the output.
        let last_start = spans[last].0;
        let list = EditList::new(vec![Edit::lines_changed(self.doc.bytes(), new_bytes)])
            .expect("a single edit is well-formed");
        let group = self.doc.history().current_group();
        self.apply_ex_edit(list, self.origin, Some(last_start))
            .expect("line filter applies cleanly");
        self.view.last_was_edit = true;
        // `'[` and `'.` at the range's first line, `']` at the last line the output put there: the line
        // above when there was none, unset above the first line (nvim's `do_filter`, #1303).
        let start = self.line_pos(first);
        self.set_ex_marks((start, start), start, Some(group));
        self.view.change_end = (first + new_lines.len())
            .checked_sub(1)
            .map(|end| self.line_pos(end));
        let nb = self.doc.bytes();
        self.view.cursor = motion::first_non_blank(nb, line_start.min(nb.len()));
        self.settle_offsets();
        self.view.curswant = self.wanted_col(); // Vim's `beginline` resets the wanted column (#1104)
        input_count
    }
}
