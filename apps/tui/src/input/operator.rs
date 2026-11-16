//! The operator model and its composition: the armed [`Op`], what it does ([`OpClass`]), and the
//! engine methods that fold an armed operator over a motion, the doubled linewise form, a mark, a search
//! object or a span command (#800). Moved out of `input/mod.rs` (#1493).

use ruse_core::{Command, MarkOp, Motion, OpKind, SearchOp, WordCase};

use super::{Feed, InputEngine};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Op {
    Delete,
    Change,
    Yank,
    /// `gu` — lowercase the operator span. `gU` = upper, `g~` = toggle, `g?` = ROT13.
    CaseLower,
    CaseUpper,
    CaseToggle,
    Rot13,
    /// `>` / `<` — indent the operator span's lines right / left (linewise).
    ShiftRight,
    ShiftLeft,
    /// `=` — reindent the operator span's lines to their bracket depth (linewise).
    Reindent,
    /// `gq` / `gw` — reflow the operator span's lines to `'textwidth'`. `Format` moves the caret to the
    /// last reformatted line; `FormatKeep` (`gw`) restores it.
    Format,
    FormatKeep,
    /// `gc` — comment / uncomment the operator span's lines with `'commentstring'` (#618).
    Comment,
    /// `!` — filter the operator span's LINES through a shell command (`!{motion}{cmd}<CR>`, `!!{cmd}`).
    /// Unlike the other operators this does not resolve to a buffer edit in the engine: the completed
    /// motion yields a [`Feed::FilterMotion`] the frontend turns into a pre-seeded `:{range}!` ex line
    /// (reusing the `:{range}!{cmd}` executor + `shell.rs`).
    Filter,
}

/// What an armed [`Op`] DOES, independent of the key that armed it. Every consumer (a motion, the doubled
/// linewise form, a search, `n`/`N`, `gn`, a mark) matches this ONCE, so no consumer can silently degrade a
/// subset of the operators into a bare move (#800).
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum OpClass {
    /// `d` / `c` / `y`.
    Text(OpKind),
    /// `gu` / `gU` / `g~` / `g?`.
    Case(WordCase),
    /// `<` (`left`) / `>`.
    Shift { left: bool },
    /// `=`.
    Reindent,
    /// `gq` / `gw` (`keep`).
    Format { keep: bool },
    /// `gc`.
    Comment,
    /// `!` — resolved by the frontend into a pre-seeded `:{range}!` line, never an edit here.
    Filter,
}

impl Op {
    pub(super) fn class(self) -> OpClass {
        match self {
            Op::Delete => OpClass::Text(OpKind::Delete),
            Op::Change => OpClass::Text(OpKind::Change),
            Op::Yank => OpClass::Text(OpKind::Yank),
            Op::CaseLower => OpClass::Case(WordCase::Downcase),
            Op::CaseUpper => OpClass::Case(WordCase::Upcase),
            Op::CaseToggle => OpClass::Case(WordCase::Toggle),
            Op::Rot13 => OpClass::Case(WordCase::Rot13),
            Op::ShiftRight => OpClass::Shift { left: false },
            Op::ShiftLeft => OpClass::Shift { left: true },
            Op::Reindent => OpClass::Reindent,
            Op::Format => OpClass::Format { keep: false },
            Op::FormatKeep => OpClass::Format { keep: true },
            Op::Comment => OpClass::Comment,
            Op::Filter => OpClass::Filter,
        }
    }
}

impl OpClass {
    /// The operator as a search / `gn` operator. `None` for the filter, which has no buffer-side form.
    pub(super) fn search_op(self) -> Option<SearchOp> {
        Some(match self {
            OpClass::Text(OpKind::Delete) => SearchOp::Delete,
            OpClass::Text(OpKind::Change) => SearchOp::Change,
            OpClass::Text(OpKind::Yank) => SearchOp::Yank,
            OpClass::Case(c) => SearchOp::Case(c),
            OpClass::Shift { left } => SearchOp::Shift { left },
            OpClass::Reindent => SearchOp::Reindent,
            OpClass::Format { keep } => SearchOp::Format { keep },
            OpClass::Comment => SearchOp::Comment,
            OpClass::Filter => return None,
        })
    }

    /// The operator as a mark-motion operator. `None` for the filter.
    pub(super) fn mark_op(self) -> Option<MarkOp> {
        Some(match self {
            OpClass::Text(OpKind::Delete) => MarkOp::Delete,
            OpClass::Text(OpKind::Change) => MarkOp::Change,
            OpClass::Text(OpKind::Yank) => MarkOp::Yank,
            OpClass::Case(c) => MarkOp::Case(c),
            OpClass::Shift { left } => MarkOp::Shift { left },
            OpClass::Reindent => MarkOp::Reindent,
            OpClass::Format { keep } => MarkOp::Format { keep },
            OpClass::Comment => MarkOp::Comment,
            OpClass::Filter => return None,
        })
    }
}

/// The operator-pending axis: an armed operator plus the RAW count typed before it (`0` = none, so `dG` and
/// `2dG` stay distinguishable — #791).
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) struct OpPending {
    pub(super) op: Op,
    pub(super) count: u32,
}

impl InputEngine {
    /// The armed operator as a search / `*` / `gn` operator: [`SearchOp::Move`] with none armed. The filter
    /// has no core form; it yields the non-mutating `Yank` its [`Feed::FilterSpan`] carries.
    pub(super) fn search_op(&self) -> SearchOp {
        match self.normal.op {
            Some(p) => p.op.class().search_op().unwrap_or(SearchOp::Yank),
            None => SearchOp::Move,
        }
    }

    /// Whether the armed operator is the `!` filter.
    pub(super) fn filter_armed(&self) -> bool {
        self.normal
            .op
            .is_some_and(|p| p.op.class() == OpClass::Filter)
    }

    /// Emit a span command (a search, `n`/`N`, `*`/`#`, `gn`, a mark): applied as usual, or under `!` handed
    /// to the frontend as a [`Feed::FilterSpan`] to number its lines (#800).
    pub(super) fn span_action(&mut self, cmd: Command) -> Feed {
        if self.filter_armed() {
            self.reset();
            Feed::FilterSpan(cmd)
        } else {
            self.action(cmd)
        }
    }

    /// Emit `m` — an operator command if one is armed, else a bare move — then clear the transient state.
    /// The repeat count is the operator count times the motion count (`3d2w` = 6), or the motion count for
    /// a bare move ([`mcount`](Self::mcount) folds every segment).
    pub(super) fn motion(&mut self, m: Motion) -> Feed {
        let total = self.mcount();
        self.op_over_motion(m, total)
    }

    /// Compose the pending operator (if any) over motion `m` with an explicit `total`, then clear the
    /// transient state. `total` is the repeat count for most motions, but the ABSOLUTE 1-based TARGET LINE
    /// for [`Motion::GotoLine`] (whose operator range is `[min,max]` of the cursor and target lines). With
    /// no operator armed it is the bare `Move(total, m)`. Shared by [`motion`](Self::motion) (computed
    /// `total`) and [`screen_op`](Self::screen_op) (the frontend-resolved `H`/`M`/`L` target line).
    pub(super) fn op_over_motion(&mut self, m: Motion, total: u32) -> Feed {
        let feed = match self.normal.op.map(|p| p.op.class()) {
            None => Feed::Cmd(Command::Move(total, m)),
            // `!{motion}` — the filter does not resolve to an edit here: hand the motion + folded count to the
            // frontend (it numbers the lines and opens the `:{range}!` cmdline).
            Some(OpClass::Filter) => Feed::FilterMotion {
                count: total,
                motion: m,
            },
            // `gu`/`gU`/`g~`/`g?` recase the operator span (no forced-wise / `cw`->`ce` rewrites apply).
            Some(OpClass::Case(case)) => Feed::Cmd(Command::CaseMotion {
                count: total,
                motion: m,
                case,
            }),
            // `>`/`<`, `=`, `gq`/`gw` over a motion — the planner works on the motion's LINES.
            Some(OpClass::Shift { left }) => Feed::Cmd(Command::ShiftMotion {
                left,
                count: total,
                motion: m,
            }),
            Some(OpClass::Reindent) => Feed::Cmd(Command::Reindent {
                count: total,
                motion: m,
            }),
            Some(OpClass::Format { keep }) => Feed::Cmd(Command::Format {
                count: total,
                motion: m,
                keep_cursor: keep,
            }),
            Some(OpClass::Comment) => Feed::Cmd(Command::Comment {
                count: total,
                motion: m,
            }),
            Some(OpClass::Text(op)) => Feed::Cmd(match (self.normal.forced_wise, op) {
                (Some(wise), op) => Command::OpForced {
                    op,
                    count: total,
                    motion: m,
                    wise,
                },
                (None, OpKind::Delete) => Command::Delete(total, m),
                // Vim `cw`/`cW` do not eat the trailing space and, unlike `ce`, changing a word's LAST char
                // changes only that char. The core's `change_range` applies that special case for
                // `WordFwd`/`BigWordFwd`, so pass the motion through unchanged (rewriting to `WordEnd` here
                // would wrongly jump into the next word from a word-final cursor).
                (None, OpKind::Change) => Command::Change(total, m),
                (None, OpKind::Yank) => Command::Yank(total, m),
            }),
        };
        self.reset();
        feed
    }

    /// A named-mark key after `` ` ``/`'`: with an operator pending it OPERATES to the mark
    /// (`` d`a ``/`d'a`, `` g~`a ``, `` >`a ``, `` =`a ``, `` gq`a ``); otherwise it JUMPS (`` `a ``/`'a`).
    /// `linewise` is the `'` form (honoured by delete/change/yank/case; the line operators are always
    /// linewise). The filter (`!'a`) hands the mark span to the frontend ([`Feed::FilterSpan`], #800).
    pub(super) fn mark_op(&mut self, name: char, linewise: bool) -> Feed {
        let cmd = match self.normal.op.map(|p| p.op.class()) {
            None if linewise => Command::GotoNamedMarkLine(name),
            None => Command::GotoNamedMark(name),
            Some(class) => Command::OpToMark {
                op: class.mark_op().unwrap_or(MarkOp::Yank),
                name,
                linewise,
            },
        };
        self.span_action(cmd)
    }

    /// Emit `gn`/`gN` — the search-match text object. Reads the last search pattern and folds in any pending
    /// operator (`dgn`/`cgn`/`ygn`, `gUgn`, `>gn`, `gqgn`); the bare form ([`SearchOp::Move`]) selects the
    /// match in Visual. With no prior search the object carries an EMPTY pattern, which the core fails (a
    /// beep, so `.` keeps the earlier change) and the frontend reports as E35, as nvim does (#1184).
    /// Emitted as a single [`Command::SearchObject`] with the pattern baked in, so `.` can replay `cgn`.
    pub(super) fn search_object(&mut self, backward: bool) -> Feed {
        let pattern = self
            .last_search
            .as_ref()
            .map(|(p, _, _)| p.clone())
            .unwrap_or_default();
        let count = self.mcount();
        self.span_action(Command::SearchObject {
            op: self.search_op(),
            count,
            pattern,
            backward,
        })
    }

    /// `*`/`#` (`whole_word`) and `g*`/`g#`: a search motion for the keyword under the cursor, carrying the
    /// count and any armed operator (`3*`, `d*`, `!#` — #1071). The planner resolves the keyword.
    pub(super) fn word_search(&mut self, forward: bool, whole_word: bool) -> Feed {
        self.span_action(Command::SearchWordUnder {
            forward,
            whole_word,
            op: self.search_op(),
            count: self.mcount(),
        })
    }

    /// Arm `op` over the next motion, keeping the RAW count typed so far as the operator count.
    pub(super) fn arm_op(&mut self, op: Op) -> Feed {
        self.normal.op = Some(OpPending {
            op,
            count: self.normal.count,
        });
        self.normal.count = 0;
        Feed::Pending
    }

    /// The doubled, LINEWISE form of the armed operator (`dd`, `3d2d`, `>>`, `==`, `guu`/`gugu`, `gqgq`,
    /// `!!`): it operates on the current `count` lines, where `count` multiplies EVERY count segment
    /// (`d3d` = 3, `2d2d` = 4 — #789). Called only with an operator armed.
    pub(super) fn op_linewise(&mut self) -> Feed {
        let n = self.mcount();
        let Some(OpPending { op, .. }) = self.normal.op else {
            unreachable!("op_linewise called without an armed operator")
        };
        let line = Motion::Line;
        let feed = match op.class() {
            OpClass::Text(OpKind::Delete) => Feed::Cmd(Command::Delete(n, line)),
            OpClass::Text(OpKind::Change) => Feed::Cmd(Command::Change(n, line)),
            OpClass::Text(OpKind::Yank) => Feed::Cmd(Command::Yank(n, line)),
            OpClass::Case(case) => Feed::Cmd(Command::CaseMotion {
                count: n,
                motion: line,
                case,
            }),
            OpClass::Shift { left: false } => Feed::Cmd(Command::ShiftRight(n)),
            OpClass::Shift { left: true } => Feed::Cmd(Command::ShiftLeft(n)),
            OpClass::Reindent => Feed::Cmd(Command::Reindent {
                count: n,
                motion: line,
            }),
            OpClass::Format { keep } => Feed::Cmd(Command::Format {
                count: n,
                motion: line,
                keep_cursor: keep,
            }),
            OpClass::Comment => Feed::Cmd(Command::Comment {
                count: n,
                motion: line,
            }),
            OpClass::Filter => Feed::FilterMotion {
                count: n,
                motion: line,
            },
        };
        self.reset();
        feed
    }

    /// An operator key: arm it, or emit its linewise form when it repeats the armed one (`dd`/`cc`/`yy`).
    /// A DIFFERENT armed operator never reaches here — the operator-pending guard aborts it first (#790).
    pub(super) fn operator(&mut self, op: Op) -> Feed {
        if self.normal.op.is_some_and(|p| p.op == op) {
            return self.op_linewise();
        }
        self.arm_op(op)
    }
}
