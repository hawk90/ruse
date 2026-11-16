//! `:g` / `:g!` / `:v` (F-009 #4): the marking pass ([`EditorState::global_marks`]) and the pass that runs
//! the payload on the marked lines ([`EditorState::global_run`]), split by payload (`d`, `s///`).

use crate::edit::{Edit, EditList};
use crate::editor::{
    find_substitutions, line_spans, resolve_line_range, EditorState, GlobalCmd, SubFlags, SubRange,
    Substitution,
};
use crate::motion;
use crate::pattern::RegexError;
use crate::pos::BytePos;
use crate::register::Register;

impl EditorState {
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

    /// Execute `:[range]g/pat/cmd` (F-009 #4) as a genuine TWO-PASS: pass 1 marks every line in range
    /// whose text the pattern matches (or does NOT match, when `negate` — `:g!` / `:v`); pass 2 runs
    /// `cmd` on the marked lines. Marking happens against the ORIGINAL buffer, so pass-2 edits never
    /// change which lines were selected — the property the acceptance names. The whole command is ONE
    /// undo group. Returns the number of lines acted on.
    ///
    /// # Errors
    /// [`RegexError`] if the `:g` pattern (or a `:g/pat/s///` sub-pattern) is unrepresentable/malformed.
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
        // PASS 2: run the command over the marked lines (marking did not mutate the buffer).
        match cmd {
            GlobalCmd::Delete => Ok(self.global_delete(marked)),
            GlobalCmd::Substitute {
                pattern: sp,
                replacement,
                flags,
            } => self.global_substitute(marked, sp, replacement, flags),
        }
    }

    /// `:g/pat/d` on the marked lines: every one deleted with its trailing newline in one transaction.
    fn global_delete(&mut self, marked: &[usize]) -> usize {
        // Re-derive the untouched buffer's line spans for pass 2 (marking did not mutate the buffer).
        let bytes = self.doc.bytes();
        let hay = bytes;
        let lines = line_spans(hay);
        // Delete each marked line including its trailing newline; disjoint + ascending, so one
        // transaction from the original offsets deletes them all atomically.
        // Adjacent lines merge into one range, so that deleting an UNTERMINATED last line can also
        // take the newline ending the line above (Vim leaves no blank line behind: `:g/b/d` on
        // `a\nb` gives `a`, #709) without overlapping the range before it.
        let mut ranges: Vec<(usize, usize)> = Vec::new();
        for &li in marked {
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
        for &li in marked {
            let (ls, le) = lines[li];
            self.view
                .registers
                .delete(None, Register::linewise(bytes[ls..le].to_vec()));
        }
        let list = EditList::new(edits).expect("whole-line deletes are disjoint and ordered");
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
        marked.len()
    }

    /// `:g/pat/s/sp/replacement/flags` on the marked lines: their substitutions found against the
    /// untouched buffer and applied as one batch. Returns the number of lines changed.
    fn global_substitute(
        &mut self,
        marked: &[usize],
        sp: &str,
        replacement: &str,
        flags: &SubFlags,
    ) -> Result<usize, RegexError> {
        let bytes = self.doc.bytes();
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
        for &li in marked {
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
