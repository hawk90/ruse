//! The Ex line commands as `EditorState` methods: `:g`, `:d`/`:y`, `:j`, `:>`/`:<`, `:m`/`:t`, `:put`,
//! `:r`, a filter, `:retab`, `:sort`, and the LSP/completion edit batch (`apply_edits`). Moved out of
//! `editor/mod.rs` (#1352); a child module, so they read the `View` and `Document` as before.

use super::*;

impl EditorState {
    /// Execute `:[range]g/pat/cmd` (F-009 #4) as a genuine TWO-PASS: pass 1 marks every line in range
    /// whose text the pattern matches (or does NOT match, when `negate` — `:g!` / `:v`); pass 2 runs
    /// `cmd` on the marked lines. Marking happens against the ORIGINAL buffer, so pass-2 edits never
    /// change which lines were selected — the property the acceptance names. The whole command is ONE
    /// undo group. Returns the number of lines acted on.
    ///
    /// # Errors
    /// [`RegexError`] if the `:g` pattern (or a `:g/pat/s///` sub-pattern) is unrepresentable/malformed.
    /// PASS 1 of `:g` in isolation: the 0-based indices of the lines in `range` whose text the `pattern`
    /// matches (or does NOT match, when `negate` — `:g!` / `:v`), evaluated against the untouched buffer.
    /// Shared by [`EditorState::global`] (the core `d`/`s` payloads) and the frontend's `:g/pat/normal`
    /// runner, which needs the SAME mark set but replays each marked line through the input engine.
    ///
    /// # Errors
    /// [`RegexError`] if the `:g` pattern is unrepresentable/malformed.
    pub fn global_marks(
        &self,
        range: SubRange,
        pattern: &str,
        negate: bool,
    ) -> Result<Vec<usize>, RegexError> {
        let opts = self.view.search_options();
        let re = crate::pattern::Regex::compile(pattern, opts)?;
        // A byte that is not UTF-8 is one char to the pattern (#1243); the marks are line indices, the
        // same in the haystack as in the buffer.
        let buf = crate::pattern::Hay::new(self.doc.bytes());
        let hay = buf.as_str();
        // `line_spans` has no span for the slot after a final newline, so a `:g`/`:v` never marks or acts on
        // it (visible under a `normal` payload, which would edit it).
        let lines = line_spans(hay);
        let count = lines.len();
        // `:g` with no range is the WHOLE FILE (unlike `:s`, whose default is the current line).
        let range = match range {
            SubRange::CurrentLine => SubRange::WholeFile,
            r => r,
        };
        let (first, last) = resolve_line_range(range, &lines, 0);
        if count == 0 {
            return Ok(Vec::new());
        }
        // CTRL-C during the marking pass abandons the whole `:g`, as nvim's `ex_global` does: nothing is
        // marked, so nothing runs.
        let abandon = |marks: Vec<usize>| {
            if crate::interrupt::interrupted() {
                Vec::new()
            } else {
                marks
            }
        };
        if !re.multiline() {
            return Ok(abandon(
                (first..=last)
                    .take_while(|_| !crate::interrupt::breakcheck())
                    .filter(|&li| {
                        let (ls, le) = lines[li];
                        re.find_at(&hay[ls..le], BytePos(0)).is_some() != negate
                    })
                    .collect(),
            ));
        }
        // A pattern that can match a line break marks the line its match STARTS on (`:g/a\nb/` marks the
        // `a` line). Searched in the whole buffer, ending in a line break so the last line's `\n` matches;
        // the leftmost match found from one line answers for every line before it.
        let owned;
        let hay: &str = if hay.ends_with('\n') {
            hay
        } else {
            owned = format!("{hay}\n");
            &owned
        };
        let mut next: Option<(usize, Option<usize>)> = None; // (searched from, leftmost whole start)
        Ok(abandon(
            (first..=last)
                .take_while(|_| !crate::interrupt::breakcheck())
                .filter(|&li| {
                    let (ls, le) = lines[li];
                    let ws = match next {
                        Some((from, ws)) if from <= ls && ws.is_none_or(|w| w >= ls) => ws,
                        _ => {
                            let ws = re.find_whole_at(hay, BytePos(ls)).map(|(w, _)| w.0);
                            next = Some((ls, ws));
                            ws
                        }
                    };
                    ws.is_some_and(|w| w <= le) != negate
                })
                .collect(),
        ))
    }

    pub fn global(
        &mut self,
        range: SubRange,
        pattern: &str,
        negate: bool,
        cmd: &GlobalCmd,
    ) -> Result<usize, RegexError> {
        // PASS 1: mark the matching (or non-matching) lines against the untouched buffer (shared with the
        // frontend's `:g/pat/normal` runner via [`EditorState::global_marks`]).
        let marked = self.global_marks(range, pattern, negate)?;
        self.global_run(&marked, cmd)
    }

    /// PASS 2 of `:g` on an already-marked set of 0-based line indices (ascending, distinct, in the buffer):
    /// what a recorded `:g` replays, so the pattern that marked them is not needed (RFC-0020).
    ///
    /// # Errors
    /// [`RegexError`] if a `:g/pat/s///` sub-pattern is unrepresentable/malformed.
    pub(crate) fn global_run(
        &mut self,
        marked: &[usize],
        cmd: &GlobalCmd,
    ) -> Result<usize, RegexError> {
        if marked.is_empty() {
            return Ok(0);
        }
        let marked = marked.to_vec();
        // Re-derive the untouched buffer's line spans for pass 2 (marking above did not mutate the buffer).
        let bytes = self.doc.bytes();
        let hay = bytes;
        let lines = line_spans(hay);

        // PASS 2: run the command over the marked lines.
        match cmd {
            GlobalCmd::Delete => {
                // Delete each marked line including its trailing newline; disjoint + ascending, so one
                // transaction from the original offsets deletes them all atomically.
                // Adjacent lines merge into one range, so that deleting an UNTERMINATED last line can also
                // take the newline ending the line above (Vim leaves no blank line behind: `:g/b/d` on
                // `a\nb` gives `a`, #709) without overlapping the range before it.
                let mut ranges: Vec<(usize, usize)> = Vec::new();
                for &li in &marked {
                    let (ls, le) = lines[li];
                    let end = if le < hay.len() { le + 1 } else { le };
                    match ranges.last_mut() {
                        Some(r) if r.1 == ls => r.1 = end,
                        _ => ranges.push((ls, end)),
                    }
                }
                if let Some(r) = ranges.last_mut() {
                    if r.1 == hay.len() && r.0 > 0 && !hay.ends_with(b"\n") {
                        r.0 -= 1;
                    }
                }
                let edits: Vec<Edit> = ranges
                    .iter()
                    .map(|&(s, e)| Edit::delete(BytePos(s), e - s))
                    .collect();
                // Vim runs `:d` once per marked line, so each deleted line goes through the unnamed register
                // and shifts the numbered ring in turn: `""` and `"1` end on the last one (#1100).
                for &li in &marked {
                    let (ls, le) = lines[li];
                    self.view
                        .registers
                        .delete(None, Register::linewise(bytes[ls..le].to_vec()));
                }
                let list =
                    EditList::new(edits).expect("whole-line deletes are disjoint and ordered");
                self.apply_ex_edit(list, self.origin, None)
                    .expect("global-delete applies cleanly");
                self.view.last_was_edit = true;
                // Like Vim, the cursor lands on the line that followed the last deleted one (the last line if
                // that was the end), at its first non-blank.
                let last = marked[marked.len() - 1] + 1 - marked.len();
                let nb = self.doc.bytes();
                let line_start = crate::pos::raw::nth_line_start(nb, last).min(nb.len());
                let line_start = if line_start == nb.len() && nb.ends_with(b"\n") {
                    nb[..nb.len() - 1]
                        .iter()
                        .rposition(|&c| c == b'\n')
                        .map_or(0, |i| i + 1)
                } else {
                    line_start
                };
                self.view.cursor = motion::first_non_blank(nb, line_start);
                self.settle_offsets();
                self.view.curswant = self.wanted_col(); // Vim's `beginline` resets the wanted column
                Ok(marked.len())
            }
            GlobalCmd::Substitute {
                pattern: sp,
                replacement,
                flags,
            } => {
                let opts = self.view.search_options();
                let sopts = crate::pattern::Options {
                    magic: opts.magic,
                    ignore_case: flags.ignore_case.unwrap_or(opts.ignore_case),
                    smart_case: flags.ignore_case.is_none() && opts.smart_case,
                };
                let sre = crate::pattern::Regex::compile(sp, sopts)?;
                let buf = crate::pattern::Hay::new(bytes);
                let lines = line_spans(buf.as_str());
                let mut subs: Vec<Substitution> = Vec::new();
                let mut consumed = None;
                for &li in &marked {
                    find_substitutions(
                        &sre,
                        &buf,
                        &lines,
                        (li, li),
                        flags.global,
                        replacement,
                        &mut consumed,
                        &mut subs,
                    );
                }
                Ok(self.apply_substitutions(&subs).lines)
            }
        }
    }

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

    /// Shared engine for `:m` (move) and `:t`/`:copy` (`copy = true` keeps the source), as one
    /// transaction over the lines it moves or copies (#1257): its lines come from the document's line
    /// index and only the block is copied, so a `:g` that runs it per line does not rescan the buffer.
    fn relocate_lines(&mut self, range: SubRange, dest: LineAddr, copy: bool) -> Option<usize> {
        let old = self.doc.bytes();
        if old.is_empty() {
            return Some(0);
        }
        let had_trailing_nl = old.ends_with(b"\n");
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

        // A move is the block deleted at its place and inserted at the destination, so the line anchors on
        // it go along (nvim's `ex_copymove` moves the marks, #1125). Without a final newline the last line
        // has none: a block holding it is cut with the `\n` before it, and one moved after it goes in after
        // a `\n`. The one shape that does not split that way (the last line moved onto an empty line just
        // above it) takes the whole-region rewrite, whose marks keep their lines. A copy is the block
        // inserted, recorded as the one line edit [`Edit::lines_changed`] finds for it.
        let mut block = old[s..e].to_vec();
        let edits = if copy {
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
        };
        let edits = edits.unwrap_or_else(|| {
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

    /// `:[line]put [reg]` — put a register's text LINEWISE as new whole line(s) after the addressed line,
    /// as one undo group. UNLIKE normal-mode `p`, a put is ALWAYS linewise (Vim `:help :put`): a charwise
    /// register's text is split on `\n` and each piece inserted as its own line — this is the
    /// linewise-FORCING rule (`:put` of a charwise `foo` opens a whole new line `foo`). The destination
    /// [`LineAddr`] is resolved as for `:m`/`:t`: `Line(0)` inserts at the very top (before line 1),
    /// `Line(n)` after 1-based line n, `Last` after the last line, and `Current` after the cursor's line
    /// (the bare `:put` default). The cursor lands on the first non-blank of the LAST inserted line
    /// (verified against nvim v0.12.4). `reg` is the register name (`None` = the unnamed register); reading
    /// it reuses the shared [`RegisterStore`], never a new store. Returns the number of lines inserted
    /// (0 — a no-op leaving the cursor put — if the register is empty).
    pub fn put_lines(&mut self, addr: LineAddr, reg: Option<char>) -> usize {
        // Reuse the shared register store: read the named slot's bytes (unnamed when `reg` is None).
        let reg_text = self.view.registers.get(reg).text().to_vec();
        if reg_text.is_empty() {
            return 0;
        }
        // Into the emptied buffer the lines go around its one line (nvim: `:put` after it, `:0put` before).
        if self.makes_the_emptied_line() {
            return self.on_the_emptied_line(|st| st.put_lines(addr, reg));
        }
        let buf = self.doc.bytes();
        let hay: &[u8] = buf;
        let orig_len = buf.len();
        let had_trailing_nl = hay.ends_with(b"\n");
        // The lines to insert: split the register text on '\n'. A normalized LINEWISE register ends with a
        // '\n', so drop the empty element it leaves after the final newline. A CHARWISE register with no
        // trailing newline still splits into whole lines — the linewise-forcing rule.
        let mut ins_lines: Vec<&[u8]> = reg_text.split(|&b| b == b'\n').collect();
        if reg_text.last() == Some(&b'\n') {
            ins_lines.pop();
        }
        if ins_lines.is_empty() {
            return 0;
        }
        // The buffer's existing lines (each WITHOUT its trailing newline), matching `relocate_lines`.
        let mut buf_lines: Vec<&[u8]> = if hay.is_empty() {
            Vec::new()
        } else {
            let mut v: Vec<&[u8]> = buf.split(|&b| b == b'\n').collect();
            if had_trailing_nl {
                v.pop(); // drop the empty element after the final '\n'
            }
            v
        };
        let nlines = buf_lines.len();
        // Destination as a 0-based insert index in `0..=nlines` (Vim `:put N` = "after line N", `:0put` =
        // top; a bare `:put` = after the cursor's line).
        let cursor_line = crate::pos::raw::line_of(buf, self.view.cursor);
        let Some(ins) = addr.insert_index(cursor_line, nlines) else {
            return 0; // a relative address off the buffer inserts nothing
        };
        let count = ins_lines.len();
        for (k, line) in ins_lines.into_iter().enumerate() {
            buf_lines.insert(ins + k, line);
        }
        let mut new_bytes: Vec<u8> = Vec::with_capacity(orig_len + reg_text.len() + 1);
        for (i, line) in buf_lines.iter().enumerate() {
            if i > 0 {
                new_bytes.push(b'\n');
            }
            new_bytes.extend_from_slice(line);
        }
        // One empty line is still a line: `\n`, not the emptied buffer (RFC-0024).
        if had_trailing_nl && !buf_lines.is_empty() {
            new_bytes.push(b'\n');
        }
        // The cursor lands on the LAST inserted line (0-based index `ins + count - 1`); compute its line
        // start now, before `new_bytes` is moved into the edit.
        let last_idx = ins + count - 1;
        let line_start: usize = new_bytes
            .split_inclusive(|&b| b == b'\n')
            .take(last_idx)
            .map(<[u8]>::len)
            .sum();
        let list = EditList::new(vec![Edit::lines_changed(self.doc.bytes(), new_bytes)])
            .expect("a single edit is well-formed");
        self.apply_ex_edit(list, self.origin, None)
            .expect("line put applies cleanly");
        self.view.last_was_edit = true;
        // Vim leaves the cursor on the first non-blank of the last inserted line.
        let nb = self.doc.bytes();
        self.view.cursor = motion::first_non_blank(nb, line_start.min(nb.len()));
        self.settle_offsets();
        self.view.curswant = self.wanted_col(); // Vim's `beginline` resets the wanted column (#1104)
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
        // The buffer's existing lines (each WITHOUT its trailing newline), matching `put_lines`.
        let mut buf_lines: Vec<&[u8]> = if hay.is_empty() {
            Vec::new()
        } else {
            let mut v: Vec<&[u8]> = buf.split(|&b| b == b'\n').collect();
            if had_trailing_nl {
                v.pop();
            }
            v
        };
        let nlines = buf_lines.len();
        let cursor_line = crate::pos::raw::line_of(buf, self.view.cursor);
        let Some(ins) = addr.insert_index(cursor_line, nlines) else {
            return 0; // a relative address off the buffer inserts nothing
        };
        let count = ins_lines.len();
        for (k, line) in ins_lines.into_iter().enumerate() {
            buf_lines.insert(ins + k, line);
        }
        let mut new_bytes: Vec<u8> = Vec::with_capacity(orig_len + text.len() + 1);
        for (i, line) in buf_lines.iter().enumerate() {
            if i > 0 {
                new_bytes.push(b'\n');
            }
            new_bytes.extend_from_slice(line);
        }
        // One empty line is still a line: `\n`, not the emptied buffer (RFC-0024).
        if had_trailing_nl && !buf_lines.is_empty() {
            new_bytes.push(b'\n');
        }
        // Cursor target line: the FIRST inserted line for a file read, the LAST for a command read.
        let target_idx = if cursor_on_last { ins + count - 1 } else { ins };
        let line_start: usize = new_bytes
            .split_inclusive(|&b| b == b'\n')
            .take(target_idx)
            .map(<[u8]>::len)
            .sum();
        let list = EditList::new(vec![Edit::lines_changed(self.doc.bytes(), new_bytes)])
            .expect("a single edit is well-formed");
        self.apply_ex_edit(list, self.origin, None)
            .expect("line read applies cleanly");
        self.view.last_was_edit = true;
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
    /// on `\n` with the trailing-newline element dropped, exactly like [`read_lines`]), as one undo group. An
    /// empty `text` deletes the range's lines (Vim: a filter that emits nothing). The cursor lands on the
    /// FIRST non-blank of the first line of the replaced region (verified vs nvim v0.12.4). Returns the number
    /// of INPUT lines filtered (0 for an empty buffer), matching Vim's "N lines filtered" report. Bytes are
    /// spliced as they are, so a buffer that is not UTF-8 filters too.
    pub fn filter_lines(&mut self, range: SubRange, text: &[u8]) -> usize {
        let buf = self.doc.bytes();
        let hay = buf;
        let orig_len = buf.len();
        let had_trailing_nl = hay.ends_with(b"\n");
        let mut lines: Vec<&[u8]> = if hay.is_empty() {
            Vec::new()
        } else {
            let mut v: Vec<&[u8]> = buf.split(|&b| b == b'\n').collect();
            if had_trailing_nl {
                v.pop();
            }
            v
        };
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
        let mut new_bytes: Vec<u8> = Vec::with_capacity(orig_len + text.len() + 1);
        for (i, line) in lines.iter().enumerate() {
            if i > 0 {
                new_bytes.push(b'\n');
            }
            new_bytes.extend_from_slice(line);
        }
        // One empty line is still a line: `\n`, not the emptied buffer (RFC-0024).
        if had_trailing_nl && !lines.is_empty() {
            new_bytes.push(b'\n');
        }
        // Cursor: the first line of the replaced region (clamped when the filter shortened the buffer).
        let target_idx = first.min(lines.len().saturating_sub(1));
        let line_start: usize = new_bytes
            .split_inclusive(|&b| b == b'\n')
            .take(target_idx)
            .map(<[u8]>::len)
            .sum();
        // Undo and redo come back to the start of the range's LAST line (#1165): nvim's `do_filter` saves
        // the change with the cursor there (`uh_cursor`), having moved it to `line2` to append the output.
        let last_start = spans[last].0;
        let list = EditList::new(vec![Edit::lines_changed(self.doc.bytes(), new_bytes)])
            .expect("a single edit is well-formed");
        self.apply_ex_edit(list, self.origin, Some(last_start))
            .expect("line filter applies cleanly");
        self.view.last_was_edit = true;
        let nb = self.doc.bytes();
        self.view.cursor = motion::first_non_blank(nb, line_start.min(nb.len()));
        self.settle_offsets();
        self.view.curswant = self.wanted_col(); // Vim's `beginline` resets the wanted column (#1104)
        input_count
    }

    /// `:[range]retab[!] [new_tabstop]` (#609), as nvim's `ex_retab` does it (v0.12.6): every run of white
    /// space that holds a `<Tab>` (with `bang`, also every run of two or more spaces) is rewritten for the
    /// same width — measured with the current `'tabstop'` — as tabs and spaces under `new_tabstop`, or as
    /// spaces only when `'expandtab'` is set. A run is left alone when the rewrite would not make it
    /// shorter, except under `'expandtab'` or when it holds a tab. The new stops are one width (a
    /// `'tabstop'`) or a list (a `'vartabstop'`, #1163); `None` keeps the stops in force. Afterwards
    /// `'vartabstop'` takes the new list when it was in use or the list has more than one stop, else
    /// `'tabstop'` takes the one width. The changed lines are one undo group, and the cursor stays on
    /// its line at the wanted column. Returns whether any line changed.
    pub fn retab(&mut self, range: SubRange, bang: bool, new_tabstop: Option<VarStops>) -> bool {
        let old = self.view.indent.tabs();
        let new = match new_tabstop.filter(|v| !v.is_empty()) {
            Some(v) => Tabs::new(old.tabstop(), v),
            None => old,
        };
        let expand = self.view.indent.style == IndentStyle::Space;
        let bytes = self.doc.bytes().to_vec();
        let hay: &[u8] = &bytes;
        let spans = line_spans(hay);
        let set_new = |st: &mut Self| {
            if let Some(v) = new_tabstop.filter(|v| !v.is_empty()) {
                if !st.view.indent.vts.is_empty() || v.count() > 1 {
                    st.view.indent.vts = v;
                } else {
                    st.view.indent.tab_width = usize::from(v.stops()[0]);
                }
            }
        };
        if spans.is_empty() {
            set_new(self);
            return false;
        }
        let cursor_line = crate::pos::raw::line_of(&bytes, self.view.cursor);
        // The wanted column the cursor returns to, measured with the tabstop in force now (Vim computes
        // it when it is needed, so a `:set ts` since the last move counts).
        if self.view.curswant != range::MAXCOL {
            self.view.curswant = self.wanted_col();
        }
        let (first, last) = resolve_line_range(range, &spans, cursor_line);
        let last = last.min(spans.len() - 1);
        let first = first.min(last);
        let mut out = bytes[..spans[first].0].to_vec();
        let mut changed = false;
        for li in first..=last {
            let (ls, le) = spans[li];
            let new_line = retab_line(&bytes[ls..le], bang, expand, old, new);
            changed |= new_line != bytes[ls..le];
            out.extend_from_slice(&new_line);
            if li < last {
                out.extend_from_slice(&bytes[le..spans[li + 1].0]);
            }
        }
        out.extend_from_slice(&bytes[spans[last].1..]);
        set_new(self);
        if !changed {
            return false;
        }
        let list = EditList::new(vec![Edit::lines_changed(self.doc.bytes(), out)])
            .expect("a single edit is well-formed");
        self.apply_ex_edit(list, self.origin, None)
            .expect("retab applies cleanly");
        self.view.last_was_edit = true;
        let nb = self.doc.bytes();
        let ls = nb
            .split_inclusive(|&b| b == b'\n')
            .take(cursor_line)
            .map(<[u8]>::len)
            .sum::<usize>()
            .min(nb.len());
        self.view.cursor = self.relocated_cursor(nb, ls);
        self.settle_offsets();
        true
    }

    /// `:[range]sort[!] [i][n][r][u] [/pattern/]` — sort the range's lines (whole file when the caller
    /// passes `WholeFile`) as one undo group, the way nvim's `ex_sort` does (v0.12.6). The sort KEY per
    /// line is the text after the first `pattern` match (the matched text itself under `r`), or the whole
    /// line; a line the pattern does not match has an empty key. Text keys compare bytewise, ignoring
    /// ASCII case under `i`. Under `n` the key is the first decimal number in it (a `-` just before it
    /// counts, and a number too large saturates); a key with no number sorts before every number. The
    /// sort is stable; `reverse` (`!`) reverses the sorted order, ties included. `unique` (`u`) then drops
    /// a line equal to the one kept before it, comparing WHOLE lines (case-folded under `i`), not keys.
    /// The pattern is matched with the `'ignorecase'` setting (no smartcase). The cursor lands on the
    /// first non-blank of the range's first line. Returns the lines removed by `u`.
    ///
    /// # Errors
    /// [`RegexError`] if `pattern` does not compile; the buffer is left as it was. A one-line range is a
    /// no-op that never compiles the pattern (Vim returns before reading the arguments).
    pub fn sort_lines(&mut self, range: SubRange, opts: &SortOptions) -> Result<usize, RegexError> {
        let bytes = self.doc.bytes();
        let hay: &[u8] = bytes;
        let had_trailing_nl = hay.ends_with(b"\n");
        let mut lines: Vec<&[u8]> = if hay.is_empty() {
            Vec::new()
        } else {
            let mut v: Vec<&[u8]> = hay.split(|&c| c == b'\n').collect();
            if had_trailing_nl {
                v.pop();
            }
            v
        };
        if lines.is_empty() {
            return Ok(0);
        }
        let spans = line_spans(hay);
        let cursor_line = crate::pos::raw::line_of(hay, self.view.cursor);
        let (first, last) = resolve_line_range(range, &spans, cursor_line);
        let last = last.min(lines.len() - 1);
        let first = first.min(last);
        if first == last {
            return Ok(0);
        }

        let search = self.view.search_options();
        let compiled = match opts.pattern.as_deref() {
            Some(p) => Some(crate::pattern::Regex::compile(
                p,
                crate::pattern::Options {
                    magic: search.magic,
                    ignore_case: search.ignore_case,
                    smart_case: false,
                },
            )?),
            None => None,
        };
        // The comparable key for one line, honoring `/pattern/`, `r`, `i`, and `n`. A byte that is not
        // UTF-8 is one char to the pattern (#1243), and text keys compare the line's bytes.
        let key_of = |line: &[u8]| -> SortKey {
            let base: &[u8] = match &compiled {
                Some(re) => {
                    let lh = crate::pattern::Hay::new(line);
                    match re
                        .find_at(lh.as_str(), BytePos(0))
                        .map(|m| lh.to_buf_match(m))
                    {
                        Some(m) if opts.use_match => &line[m.start.0..m.end.0],
                        Some(m) => &line[m.end.0..],
                        None => b"",
                    }
                }
                None => line,
            };
            match opts.key {
                SortKind::Decimal | SortKind::Hex | SortKind::Octal | SortKind::Binary => {
                    SortKey::Num(first_number(&crate::pattern::spell(base), opts.key))
                }
                SortKind::Float => SortKey::Float(first_float(&crate::pattern::spell(base))),
                SortKind::Locale => SortKey::Locale(crate::pattern::spell(base).into_owned()),
                SortKind::Text if opts.ignore_case => SortKey::Text(base.to_ascii_lowercase()),
                SortKind::Text => SortKey::Text(base.to_vec()),
            }
        };

        let mut keyed: Vec<(SortKey, &[u8])> = lines[first..=last]
            .iter()
            .map(|&l| (key_of(l), l))
            .collect();
        keyed.sort_by(|a, b| a.0.compare(&b.0)); // stable (Vim breaks ties by line number)
        if opts.reverse {
            keyed.reverse();
        }
        let mut seg: Vec<&[u8]> = keyed.into_iter().map(|(_, l)| l).collect();
        if opts.unique {
            if opts.ignore_case {
                seg.dedup_by(|a, b| a.eq_ignore_ascii_case(b));
            } else {
                seg.dedup();
            }
        }
        let removed = (last - first + 1) - seg.len();
        lines.splice(first..=last, seg);

        let mut new_bytes = lines.join(&b'\n');
        // One empty line is still a line: `\n`, not the emptied buffer (RFC-0024).
        if had_trailing_nl && !lines.is_empty() {
            new_bytes.push(b'\n');
        }
        let line_start = new_bytes
            .split_inclusive(|&b| b == b'\n')
            .take(first)
            .map(<[u8]>::len)
            .sum::<usize>()
            .min(new_bytes.len());
        let list = EditList::new(vec![Edit::lines_changed(self.doc.bytes(), new_bytes)])
            .expect("a single edit is well-formed");
        self.apply_ex_edit(list, self.origin, None)
            .expect("sort applies cleanly");
        self.view.last_was_edit = true;
        let nb = self.doc.bytes();
        self.view.cursor = motion::first_non_blank(nb, line_start.min(nb.len()));
        self.settle_offsets();
        self.view.curswant = self.wanted_col(); // Vim's `beginline` resets the wanted column (#1104)
        Ok(removed)
    }

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

/// One line of [`EditorState::retab`] (nvim's `ex_retab` loop): columns are measured with the `old` stops,
/// and a rewritten run is laid out with the `new` ones (`tabstop_fromto`). The line is walked by cluster with the cells
/// the renderer paints ([`range::cluster_cells`]: `<ff>` for a byte that is not UTF-8 is 4, `^A` 2), so a
/// line that is not UTF-8 is retabbed too (#1252).
fn retab_line(line: &[u8], bang: bool, expand: bool, old_ts: Tabs, new_ts: Tabs) -> Vec<u8> {
    let mut out = Vec::with_capacity(line.len());
    let (mut vcol, mut start_vcol, mut start) = (0usize, 0usize, 0usize);
    let (mut got_tab, mut num_spaces) = (false, 0usize);
    let mut copied = 0usize;
    let mut col = 0usize;
    loop {
        let c = line.get(col).copied();
        if matches!(c, Some(b' ' | b'\t')) {
            if !got_tab && num_spaces == 0 {
                start_vcol = vcol;
                start = col;
            }
            if c == Some(b' ') {
                num_spaces += 1;
            } else {
                got_tab = true;
            }
        } else {
            if got_tab || (bang && num_spaces > 1) {
                let len = vcol - start_vcol;
                let (mut tabs, mut spaces) = (0, len);
                if !expand {
                    (tabs, spaces) = new_ts.fromto(start_vcol, vcol);
                }
                if expand || got_tab || tabs + spaces < len {
                    out.extend_from_slice(&line[copied..start]);
                    out.extend(std::iter::repeat_n(b'\t', tabs));
                    out.extend(std::iter::repeat_n(b' ', spaces));
                    copied = col;
                }
            }
            got_tab = false;
            num_spaces = 0;
        }
        if c.is_none() {
            break;
        }
        let (next, w) = range::cluster_cells(line, col, line.len(), vcol, old_ts);
        vcol += w;
        col = next;
    }
    out.extend_from_slice(&line[copied..]);
    out
}

/// A line's comparable sort key. All keys in one sort share a variant. `Num(None)` (no number in the key)
/// orders before every number, as in Vim.
#[derive(Clone, PartialEq)]
enum SortKey {
    Num(Option<i64>),
    Float(f64),
    Text(Vec<u8>),
    Locale(String),
}

impl SortKey {
    fn compare(&self, other: &SortKey) -> std::cmp::Ordering {
        match (self, other) {
            (SortKey::Num(a), SortKey::Num(b)) => a.cmp(b),
            // nvim compares with `==`/`>`, which a NaN defeats; a total order keeps the sort well-defined.
            (SortKey::Float(a), SortKey::Float(b)) => a.total_cmp(b),
            (SortKey::Text(a), SortKey::Text(b)) => a.cmp(b),
            (SortKey::Locale(a), SortKey::Locale(b)) => locale_cmp(a, b),
            _ => std::cmp::Ordering::Equal,
        }
    }
}

/// The `:sort l` order (see [`SortKind::Locale`]): case-insensitive first, then lowercase before
/// uppercase, then bytewise.
fn locale_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    let fold = |s: &str| s.chars().flat_map(char::to_lowercase).collect::<String>();
    fold(a).cmp(&fold(b)).then_with(|| {
        let case = |s: &str| s.chars().map(char::is_uppercase).collect::<Vec<_>>();
        case(a).cmp(&case(b)).then_with(|| a.cmp(b))
    })
}

/// The first number in `s` for `:sort n`/`x`/`o`/`b` (nvim's `ex_sort` + `vim_str2nr`): skip to the
/// first digit of the base (for `x` any hex digit, for `b` a `0` or `1`), take a `-` just before it, then
/// read the number, after an optional `0x`/`0b` prefix for `x`/`b`. `None` when `s` has no such digit. A
/// number past the `i64` range saturates to its bound.
fn first_number(s: &str, kind: SortKind) -> Option<i64> {
    let b = s.as_bytes();
    let starts = |c: &u8| match kind {
        SortKind::Hex => c.is_ascii_hexdigit(),
        SortKind::Binary => matches!(c, b'0' | b'1'),
        _ => c.is_ascii_digit(),
    };
    let i = b.iter().position(starts)?;
    let negative = i > 0 && b[i - 1] == b'-';
    let (radix, prefix) = match kind {
        SortKind::Hex => (16, b'x'),
        SortKind::Octal => (8, 0),
        SortKind::Binary => (2, b'b'),
        _ => (10, 0),
    };
    let digit = |c: u8| char::from(c).to_digit(radix);
    let mut at = i;
    if prefix != 0
        && b[at] == b'0'
        && b.get(at + 1).map(u8::to_ascii_lowercase) == Some(prefix)
        && b.get(at + 2).is_some_and(|&c| digit(c).is_some())
    {
        at += 2;
    }
    let magnitude = b[at..].iter().map_while(|&c| digit(c)).fold(0u64, |n, d| {
        n.saturating_mul(u64::from(radix))
            .saturating_add(u64::from(d))
    });
    Some(if negative {
        0i64.checked_sub_unsigned(magnitude).unwrap_or(i64::MIN)
    } else {
        i64::try_from(magnitude).unwrap_or(i64::MAX)
    })
}

/// The number at the start of `s` for `:sort f` (nvim's `ex_sort`): blanks and one `+` are skipped, an
/// empty rest sorts before every number (`-DBL_MAX`), and otherwise the value is what C's `strtod` reads
/// (0 when nothing parses).
fn first_float(s: &str) -> f64 {
    let s = s.trim_start_matches([' ', '\t']);
    let s = s
        .strip_prefix('+')
        .map_or(s, |r| r.trim_start_matches([' ', '\t']));
    if s.is_empty() {
        return -f64::MAX;
    }
    strtod_prefix(s)
}

/// The value of the longest prefix of `s` that C's `strtod` accepts (after leading whitespace): an
/// optional sign, then `inf`/`infinity`, `nan`, a hex number (`0x1.8p3`) or a decimal one (`1.5e-3`).
fn strtod_prefix(s: &str) -> f64 {
    let s = s.trim_start();
    let b = s.as_bytes();
    let (sign, mut i) = match b.first() {
        Some(b'-') => (-1.0, 1),
        Some(b'+') => (1.0, 1),
        _ => (1.0, 0),
    };
    let rest = &s[i..];
    let starts_ci = |w: &str| rest.len() >= w.len() && rest[..w.len()].eq_ignore_ascii_case(w);
    if starts_ci("inf") {
        return sign * f64::INFINITY;
    }
    if starts_ci("nan") {
        return f64::NAN;
    }
    let digits_from = |i: &mut usize, hex: bool| {
        let start = *i;
        while b.get(*i).is_some_and(|c| {
            if hex {
                c.is_ascii_hexdigit()
            } else {
                c.is_ascii_digit()
            }
        }) {
            *i += 1;
        }
        *i - start
    };
    // Hex: `0x` hexdigits [`.` hexdigits] [`p` exponent].
    if b.get(i) == Some(&b'0') && matches!(b.get(i + 1), Some(b'x' | b'X')) {
        let mut j = i + 2;
        let mut mantissa = 0f64;
        let int_start = j;
        let int_len = digits_from(&mut j, true);
        for &c in &b[int_start..int_start + int_len] {
            mantissa = mantissa * 16.0 + f64::from(char::from(c).to_digit(16).unwrap_or(0));
        }
        let mut frac_len = 0;
        if b.get(j) == Some(&b'.') {
            let mut k = j + 1;
            let fs = k;
            frac_len = digits_from(&mut k, true);
            let mut scale = 1.0 / 16.0;
            for &c in &b[fs..fs + frac_len] {
                mantissa += f64::from(char::from(c).to_digit(16).unwrap_or(0)) * scale;
                scale /= 16.0;
            }
            if int_len + frac_len > 0 {
                j = k;
            }
        }
        if int_len + frac_len == 0 {
            // Only the `0` parses.
            return sign * 0.0;
        }
        let mut exp = 0i32;
        if matches!(b.get(j), Some(b'p' | b'P')) {
            let mut k = j + 1;
            let neg = match b.get(k) {
                Some(b'-') => {
                    k += 1;
                    true
                }
                Some(b'+') => {
                    k += 1;
                    false
                }
                _ => false,
            };
            let es = k;
            if digits_from(&mut k, false) > 0 {
                let e: i32 = s[es..k].parse().unwrap_or(i32::MAX);
                exp = if neg { -e } else { e };
            }
        }
        return sign * mantissa * 2f64.powi(exp);
    }
    // Decimal: digits [`.` digits] [`e` exponent], with at least one digit.
    let start = i;
    let int_len = digits_from(&mut i, false);
    let mut frac_len = 0;
    if b.get(i) == Some(&b'.') {
        let mut k = i + 1;
        frac_len = digits_from(&mut k, false);
        if int_len + frac_len > 0 {
            i = k;
        }
    }
    if int_len + frac_len == 0 {
        return 0.0;
    }
    if matches!(b.get(i), Some(b'e' | b'E')) {
        let mut k = i + 1;
        if matches!(b.get(k), Some(b'-' | b'+')) {
            k += 1;
        }
        if digits_from(&mut k, false) > 0 {
            i = k;
        }
    }
    sign * s[start..i].parse::<f64>().unwrap_or(0.0)
}

/// The text `:m` (or `:t`, with `copy`) makes of the non-empty `old`, by the line model: its lines with
/// `first..=last` (0-based, in range) moved or copied to before line `ins` (`0..=lines`), the final
/// newline kept as it was. [`EditorState::relocate_lines`] edits only the block; this is the rewrite
/// for the one shape it cannot split, and the reference its tests hold it to.
pub(super) fn relocated_by_lines(
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
