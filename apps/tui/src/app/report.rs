//! The line-count reports a command gives, and the `'report'` threshold they pass (#1276).
//!
//! Ground truth: nvim v0.12.6 `--clean`, headless, `execute('messages')` after each command over a ten-line
//! buffer at `'report'` 0, 2 and 10. A report is given only when its count is MORE than `'report'`
//! (default 2), so by default one or two lines say nothing:
//!
//! - lines added or removed (nvim's `msgmore`): `1 more line`, `3 more lines`, `1 line less`, `3 fewer
//!   lines`, after `d`, `c`, `p`, `gq`, `:d`, `:t`, `:pu`, `:sort u` and a `:g`;
//! - a yank: `3 lines yanked`, `block of 3 lines yanked`, `… into "a` for a named register; a charwise
//!   yank inside one line counts no lines, and the black hole register says nothing;
//! - `3 lines moved` (`:m`), `3 lines >ed 2 times` (`>`, `<`, `:>`);
//! - `3 substitutions on 2 lines`: counted by substitutions, not lines; the count-only `:s///n` report
//!   (`3 matches on 2 lines`) is always given.
//!
//! `J`/`:j` and `x` give none.

/// nvim's default `'report'`.
pub(crate) const REPORT_DEFAULT: i64 = 2;

fn passes(count: usize, report: i64) -> bool {
    i64::try_from(count).unwrap_or(i64::MAX) > report
}

fn lines(n: usize) -> &'static str {
    if n == 1 {
        "line"
    } else {
        "lines"
    }
}

/// `delta` lines more (positive) or fewer (negative) than before (nvim's `msgmore`).
pub(crate) fn more(delta: i64, report: i64) -> Option<String> {
    let n = usize::try_from(delta.unsigned_abs()).unwrap_or(usize::MAX);
    if delta == 0 || !passes(n, report) {
        return None;
    }
    Some(match (delta > 0, n) {
        (true, 1) => "1 more line".to_string(),
        (true, _) => format!("{n} more lines"),
        (false, 1) => "1 line less".to_string(),
        (false, _) => format!("{n} fewer lines"),
    })
}

/// [`more`] for a buffer that went from `before` to `after` lines.
pub(crate) fn line_delta(before: usize, after: usize, report: i64) -> Option<String> {
    let to_i = |n: usize| i64::try_from(n).unwrap_or(i64::MAX);
    more(to_i(after) - to_i(before), report)
}

/// `n` lines yanked into `reg` (`None`: the unnamed register), as a block when `block`.
pub(crate) fn yanked(n: usize, block: bool, reg: Option<char>, report: i64) -> Option<String> {
    if reg == Some('_') || !passes(n, report) {
        return None;
    }
    let what = if block { "block of " } else { "" };
    let into = match reg {
        Some(c) if c != '"' => format!(" into \"{c}"),
        _ => String::new(),
    };
    Some(format!("{what}{n} {} yanked{into}", lines(n)))
}

/// `n` lines moved (`:m`).
pub(crate) fn moved(n: usize, report: i64) -> Option<String> {
    passes(n, report).then(|| format!("{n} {} moved", lines(n)))
}

/// `n` lines shifted `amount` times, left for `<`.
pub(crate) fn shifted(n: usize, left: bool, amount: u32, report: i64) -> Option<String> {
    let op = if left { '<' } else { '>' };
    let times = if amount == 1 { "time" } else { "times" };
    passes(n, report).then(|| format!("{n} {} {op}ed {amount} {times}", lines(n)))
}

/// `subs` substitutions on `on_lines` lines (`:s`, `&`, `g&`, `:&&`, the `:s///c` loop).
pub(crate) fn substituted(subs: usize, on_lines: usize, report: i64) -> Option<String> {
    let what = if subs == 1 {
        "substitution"
    } else {
        "substitutions"
    };
    passes(subs, report).then(|| format!("{subs} {what} on {on_lines} {}", lines(on_lines)))
}

/// The line-count report a Normal or Visual command gives once it ran (#1276, see `crate::app::report`).
pub(crate) enum NormalReport {
    /// Lines added or removed (`d`, `c`, `p`, `gq`): the line count before.
    Delta(usize),
    /// A yank into this register (`None`: the unnamed one).
    Yank(Option<char>),
    /// A shift of `amount` levels, over `lines` lines when they are known before it runs (`>>`, `3<<`);
    /// else the `[`..`]` marks it leaves tell.
    Shift {
        left: bool,
        amount: u32,
        lines: Option<usize>,
    },
}

impl NormalReport {
    pub(crate) fn before(
        cmd: &ruse_core::Command,
        ws: &ruse_core::Workspace,
    ) -> Option<NormalReport> {
        use ruse_core::{MarkOp, OpKind, SearchOp, VisualOp};
        let delta = || {
            Some(NormalReport::Delta(crate::app::dispatch::line_count(
                ws.focused().doc.bytes(),
            )))
        };
        let yank = || Some(NormalReport::Yank(ws.focused().view.pending_register()));
        let shift = |left, amount| {
            Some(NormalReport::Shift {
                left,
                amount,
                lines: None,
            })
        };
        // `{count}>>`: the count's lines from the cursor's, as far as the buffer goes.
        let count_lines = |n: u32| {
            let pane = ws.focused();
            let b = pane.doc.bytes();
            let cur = ruse_core::pos::line_of(b, pane.view.cursor());
            let total = crate::app::dispatch::line_count(b);
            (n.max(1) as usize).min(total.saturating_sub(cur))
        };
        match cmd {
            ruse_core::Command::Yank(..)
            | ruse_core::Command::YankSelection
            | ruse_core::Command::OpForced {
                op: OpKind::Yank, ..
            }
            | ruse_core::Command::OpToMark {
                op: MarkOp::Yank, ..
            }
            | ruse_core::Command::VisualOp(VisualOp::Lines(OpKind::Yank))
            | ruse_core::Command::Search {
                op: SearchOp::Yank, ..
            } => yank(),
            ruse_core::Command::Search {
                op: SearchOp::Delete | SearchOp::Change,
                ..
            } => delta(),
            ruse_core::Command::Search {
                op: SearchOp::Shift { left },
                ..
            } => shift(*left, 1),
            ruse_core::Command::Delete(..)
            | ruse_core::Command::Change(..)
            | ruse_core::Command::DeleteSelection
            | ruse_core::Command::ChangeSelection
            | ruse_core::Command::Format { .. }
            | ruse_core::Command::Paste { .. }
            | ruse_core::Command::PasteSelection { .. }
            | ruse_core::Command::PasteIndent { .. }
            | ruse_core::Command::PasteBlockInner { .. }
            | ruse_core::Command::OpForced { .. }
            | ruse_core::Command::OpToMark {
                op: MarkOp::Delete | MarkOp::Change | MarkOp::Format { .. },
                ..
            }
            | ruse_core::Command::VisualOp(
                VisualOp::Lines(OpKind::Delete | OpKind::Change)
                | VisualOp::BlockToEol(OpKind::Delete | OpKind::Change)
                | VisualOp::Format,
            ) => delta(),
            ruse_core::Command::ShiftRight(n) | ruse_core::Command::ShiftLeft(n) => {
                Some(NormalReport::Shift {
                    left: matches!(cmd, ruse_core::Command::ShiftLeft(_)),
                    amount: 1,
                    lines: Some(count_lines(*n)),
                })
            }
            ruse_core::Command::ShiftMotion { left, .. }
            | ruse_core::Command::OpToMark {
                op: MarkOp::Shift { left },
                ..
            } => shift(*left, 1),
            ruse_core::Command::VisualOp(VisualOp::Shift { left, count }) => shift(*left, *count),
            _ => None,
        }
    }

    pub(crate) fn give(
        self,
        ws: &ruse_core::Workspace,
        status: &mut crate::app::messages::Status,
        report: i64,
    ) {
        let pane = ws.focused();
        let bytes = pane.doc.bytes();
        // The lines the `[`..`]` marks of the yank or shift span.
        let marked = || {
            let (s, e) = (pane.view.change_mark_start()?, pane.view.change_mark_end()?);
            let (s, e) = (
                ruse_core::pos::line_of(bytes, s),
                ruse_core::pos::line_of(bytes, e),
            );
            Some(e.saturating_sub(s) + 1)
        };
        let msg = match self {
            NormalReport::Delta(_) if bytes.is_empty() => {
                // nvim says this instead of the count when the buffer is left empty.
                status.echo("--No lines in buffer--");
                return;
            }
            NormalReport::Delta(before) => {
                line_delta(before, crate::app::dispatch::line_count(bytes), report)
            }
            NormalReport::Yank(reg) => {
                let kind = ws
                    .registers()
                    .get(reg.map(|c| c.to_ascii_lowercase()))
                    .kind();
                let n = marked().unwrap_or(0);
                let charwise = kind == ruse_core::RegKind::Charwise;
                // A charwise yank that takes a line's newline (`v$y`) reaches the next line, as nvim's
                // operator end does; one inside one line counts no lines (nvim's `op_yank`).
                let ends_at_newline = pane
                    .view
                    .change_mark_end()
                    .is_some_and(|e| bytes.get(e.0) == Some(&b'\n'));
                let n = n + usize::from(charwise && ends_at_newline);
                let n = if charwise && n == 1 { 0 } else { n };
                yanked(n, kind == ruse_core::RegKind::Blockwise, reg, report)
            }
            NormalReport::Shift {
                left,
                amount,
                lines,
            } => shifted(lines.or_else(marked).unwrap_or(0), left, amount, report),
        };
        if let Some(msg) = msg {
            status.info(msg);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wording_matches_nvim() {
        assert_eq!(more(1, 0).as_deref(), Some("1 more line"));
        assert_eq!(more(3, 0).as_deref(), Some("3 more lines"));
        assert_eq!(more(-1, 0).as_deref(), Some("1 line less"));
        assert_eq!(more(-3, 0).as_deref(), Some("3 fewer lines"));
        assert_eq!(yanked(1, false, None, 0).as_deref(), Some("1 line yanked"));
        assert_eq!(
            yanked(3, false, Some('a'), 0).as_deref(),
            Some("3 lines yanked into \"a")
        );
        assert_eq!(
            yanked(3, true, None, 0).as_deref(),
            Some("block of 3 lines yanked")
        );
        assert_eq!(moved(1, 0).as_deref(), Some("1 line moved"));
        assert_eq!(
            shifted(1, false, 1, 0).as_deref(),
            Some("1 line >ed 1 time")
        );
        assert_eq!(
            shifted(3, true, 2, 0).as_deref(),
            Some("3 lines <ed 2 times")
        );
        assert_eq!(
            substituted(1, 1, 0).as_deref(),
            Some("1 substitution on 1 line")
        );
        assert_eq!(
            substituted(2, 1, 0).as_deref(),
            Some("2 substitutions on 1 line")
        );
    }

    #[test]
    fn a_count_must_pass_report() {
        assert_eq!(more(-2, 2), None);
        assert_eq!(more(-3, 2).as_deref(), Some("3 fewer lines"));
        assert_eq!(yanked(2, false, None, 2), None);
        assert_eq!(yanked(5, false, Some('_'), 0), None);
        assert_eq!(moved(10, 10), None);
        assert_eq!(substituted(2, 2, 2), None);
        assert_eq!(more(0, 0), None);
    }
}
