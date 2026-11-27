//! A `:s///c` run from its recorded answers (#1628): which matches change, and where nvim's `do_sub`
//! (v0.12.6, the `i1628_*` oracle fixtures) leaves the cursor, `'.` and `']` once the loop ends.

use super::{line_spans, resolve_line_range, SubOutcome, SubRange, Substitution};
use crate::editor::SubAnswer;
use crate::{motion, EditorState};

/// The substitution (an index into the applied set) `'.` goes to, on the line it ends on (`at_end`) or
/// the line it starts on.
#[derive(Clone, Copy)]
pub(super) struct DotAt {
    pub(super) sub: usize,
    pub(super) at_end: bool,
}

impl EditorState {
    /// Apply the matches `subs` of the `:s` over `range` (found by
    /// [`EditorState::substitute_preview`]) that `answers` accepted, one answer per match prompted for,
    /// as one undo group. Answers past the last match are ignored; a match after the last answer was
    /// never prompted for and is not replaced (unless an `a` came first). Where it leaves things, as
    /// nvim does:
    /// - the cursor: on the last match prompted for when it was skipped or quit at (`n`, `q`), at the
    ///   start of the line the replacement ends on when it was taken (`y`, `l`), and on the first
    ///   non-blank of that line after an `a` (which stops prompting, so `do_sub` ends as a plain `:s`);
    /// - `'.`: the line the last `y`/`l` replacement ends on, or the line the first `a` replacement
    ///   starts on;
    /// - `']`: the range's last line, or with `l` the line that match is on.
    pub(crate) fn apply_confirmed(
        &mut self,
        subs: &[Substitution],
        answers: &[SubAnswer],
        range: SubRange,
    ) -> SubOutcome {
        let mut accepted: Vec<Substitution> = Vec::new();
        let mut dot = None;
        let mut stop = None;
        for (i, (s, &answer)) in subs.iter().zip(answers).enumerate() {
            stop = Some((i, answer));
            match answer {
                SubAnswer::Yes | SubAnswer::Last => {
                    dot = Some(DotAt {
                        sub: accepted.len(),
                        at_end: true,
                    });
                    accepted.push(s.clone());
                }
                SubAnswer::All => {
                    dot = Some(DotAt {
                        sub: accepted.len(),
                        at_end: false,
                    });
                    accepted.extend_from_slice(&subs[i..]);
                }
                SubAnswer::No | SubAnswer::Quit => {}
            }
            if !answer.asks_on() {
                break;
            }
        }
        let none = SubOutcome {
            replacements: 0,
            lines: 0,
        };
        let Some((stop, answer)) = stop else {
            return none;
        };
        let bytes = self.doc.bytes();
        let cursor_line = crate::pos::raw::line_of(bytes, self.view.cursor);
        let (first, mut last) = resolve_line_range(range, &line_spans(bytes), cursor_line);
        if answer == SubAnswer::Last {
            last = subs[stop].line;
        }
        // Where the stop match starts once the replacements before it are in (only `n`/`q` reads it, and
        // then every accepted match is before it).
        let shift: isize = accepted
            .iter()
            .map(|s| s.replacement.len() as isize - (s.end.0 - s.start.0) as isize)
            .sum();
        let out = if accepted.is_empty() {
            none
        } else {
            self.apply_substitutions_marked(&accepted, Some((first, last)), dot)
        };
        let b = self.doc.bytes();
        self.view.cursor = match answer {
            SubAnswer::All => return out,
            SubAnswer::Yes | SubAnswer::Last => motion::line_start(b, self.view.cursor),
            SubAnswer::No | SubAnswer::Quit => {
                (subs[stop].start.0 as isize + shift).max(0) as usize
            }
        }
        .min(b.len());
        self.settle_offsets();
        self.view.curswant = self.wanted_col();
        out
    }
}
