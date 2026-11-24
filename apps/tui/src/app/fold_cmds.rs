//! Commands over closed folds (#819). The core counts buffer lines and knows nothing of folds (folds are
//! frontend state, INV-DOC-VIEW), so the session rewrites a command typed over a closed fold into the one
//! that does what nvim does there, before the core runs it:
//!
//! - `j`/`k`/`+`/`-` count a closed fold as one line ([`folds::line_down`], [`folds::line_up`]); the move
//!   becomes a plain one over the buffer lines that skip it, so the wanted column still comes from the core.
//! - An operator whose range starts or ends in a closed fold takes the whole fold (nvim's
//!   `do_pending_operator` "include closed folds as a whole"): `dd`, `x`, `dw`, `D` delete its lines,
//!   `yy`/`>>`/`gUU` act on them, `cl`/`C` and `yl`/`Y` change or yank them characterwise.
//! - `o` opens below the fold's last line, `O` above its first; a linewise `p`/`P` puts below or above
//!   the fold (nvim's `n_opencmd` and `do_put`).
//! - A search or `n` from inside a closed fold starts past it (nvim's `do_search`).
//!
//! The rewritten command is what the trace records, so a replay needs no fold state. [`opens_fold`] names
//! the commands after which nvim opens the fold the cursor lands in ('foldopen' at its default).

use ruse_core::{Command, Motion, RegKind, Workspace};

use crate::folds::{self, Fold};
use crate::line_index::LineIndex;

/// What to run for a command typed over closed folds.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum FoldStep {
    /// Run this command.
    Run(Command),
    /// Put the cursor on byte `at` (the start of a line), then run the command.
    PlaceThen(usize, Command),
    /// The move fails, as nvim's does there (`j` on a closed fold that reaches the last line): run
    /// nothing and beep.
    Fail,
}

/// The direction of a line motion that counts a closed fold as one line: `Some(true)` down, `Some(false)`
/// up.
fn vertical(m: Motion) -> Option<bool> {
    match m {
        Motion::Down | Motion::DownFirstNonBlank => Some(true),
        Motion::Up | Motion::UpFirstNonBlank => Some(false),
        _ => None,
    }
}

/// Whether an operator over `m` acts on whole lines (the core's linewise motions, `editor::range`).
fn linewise(m: Motion) -> bool {
    matches!(
        m,
        Motion::Line
            | Motion::Up
            | Motion::Down
            | Motion::GotoLine
            | Motion::LastLine
            | Motion::DownFirstNonBlank
            | Motion::UpFirstNonBlank
            | Motion::LineUnderscore
            | Motion::GotoPercent
    )
}

/// The buffer, its closed folds and the cursor line a command over them is rewritten against.
struct Over<'a> {
    ws: &'a Workspace,
    folds: &'a [Fold],
    line_idx: &'a LineIndex,
    snapshot: &'a [u8],
    /// The cursor line.
    cur: usize,
    /// The last buffer line.
    last: usize,
}

impl Over<'_> {
    /// The first line of the closed fold that holds `l`, else `l`.
    fn start_of(&self, l: usize) -> usize {
        folds::closed_range(self.folds, l).map_or(l, |r| r.0)
    }

    /// The last line of the closed fold that holds `l`, else `l`.
    fn end_of(&self, l: usize) -> usize {
        folds::closed_range(self.folds, l).map_or(l, |r| r.1)
    }

    /// Whether a closed fold holds a line of `lo..=hi`: else the command runs as typed.
    fn touches(&self, lo: usize, hi: usize) -> bool {
        self.folds
            .iter()
            .any(|f| f.closed && f.start <= hi && lo <= f.end)
    }

    /// The byte that starts line `l`.
    fn at(&self, l: usize) -> usize {
        self.line_idx.nth_line_start(l)
    }

    /// Run `c` from line `l`: as typed when the cursor is on it, else after placing the cursor there.
    fn place(&self, l: usize, c: Command) -> FoldStep {
        if l == self.cur {
            FoldStep::Run(c)
        } else {
            FoldStep::PlaceThen(self.at(l), c)
        }
    }
}

/// Where a rewrite over closed folds lands: the command runs as typed, the move fails, or it reaches `T`.
enum Reach<T> {
    AsTyped,
    Fail,
    To(T),
}

/// Rewrite `cmd` for the closed folds in `folds` (see the module docs). `snapshot` is the focused buffer and
/// `line_idx` its line index, both as they are before `cmd` runs.
pub(crate) fn resolve(
    cmd: Command,
    ws: &Workspace,
    folds: &[Fold],
    line_idx: &LineIndex,
    snapshot: &[u8],
) -> FoldStep {
    use ruse_core::Mode;
    let mode = ws.focused().view.mode();
    let normal = mode == Mode::Normal;
    if !normal && !matches!(mode, Mode::Visual { .. } | Mode::Select { .. }) {
        return FoldStep::Run(cmd);
    }
    let cur = line_idx.line_of(ws.focused().view.cursor().0);
    let last = line_idx
        .line_of(ruse_core::motion::off_phantom(snapshot, ruse_core::BytePos(snapshot.len())).0);
    let c = Over {
        ws,
        folds,
        line_idx,
        snapshot,
        cur,
        last,
    };

    if let Some(from) = search_start(&c, &cmd) {
        return FoldStep::PlaceThen(from, cmd);
    }
    // `j`/`k`/`+`/`-` (and in Visual mode too).
    if let Command::Move(n, m) = cmd {
        return line_move(&c, cmd, n, m);
    }
    if !normal {
        return FoldStep::Run(cmd);
    }
    match cmd {
        Command::OpenBelow
        | Command::OpenAbove
        | Command::Paste { .. }
        | Command::PasteIndent { .. } => open_or_put(&c, cmd),
        _ => operator(&c, cmd),
    }
}

/// A search from inside a closed fold finds no other match in it (nvim's `do_search`): forward it starts
/// from the end of the fold's last line, backward from the start of its first. A word motion steps over
/// it the same way (nvim's `fwd_word`, `end_word`, `bck_word`). The byte to start `cmd` from, or `None`
/// when it is no such command or the cursor is in no closed fold.
fn search_start(c: &Over, cmd: &Command) -> Option<usize> {
    let backward = match cmd {
        Command::SearchNext(_) => Some(false),
        Command::SearchPrev(_) => Some(true),
        Command::Search { op, backward, .. } if *op == ruse_core::SearchOp::Move => Some(*backward),
        Command::Move(
            _,
            Motion::WordFwd | Motion::BigWordFwd | Motion::WordEnd | Motion::BigWordEnd,
        ) => Some(false),
        Command::Move(_, Motion::WordBack | Motion::BigWordBack) => Some(true),
        _ => None,
    };
    let (Some(backward), Some((s, e))) = (backward, folds::closed_range(c.folds, c.cur)) else {
        return None;
    };
    if backward {
        return Some(c.at(s));
    }
    let ls = c.at(e);
    let le = ruse_core::pos::line_end(c.snapshot, ruse_core::BytePos(ls)).0;
    let mut last_char = le;
    while last_char > ls {
        last_char -= 1;
        if c.snapshot[last_char] & 0xC0 != 0x80 {
            break;
        }
    }
    Some(last_char)
}

/// The line `n` lines down (`down`) or up from the cursor, a closed fold counted as one line.
fn vertical_to(c: &Over, down: bool, n: usize) -> Reach<usize> {
    let plain = if down {
        (c.cur + n).min(c.last)
    } else {
        c.cur.saturating_sub(n)
    };
    if !c.touches(c.cur.min(plain), c.cur.max(plain)) {
        return Reach::AsTyped;
    }
    let to = if down {
        folds::line_down(c.folds, c.cur, n, c.last)
    } else {
        folds::line_up(c.folds, c.cur, n)
    };
    to.map_or(Reach::Fail, Reach::To)
}

/// `cmd`, the move `Move(n, m)`: a vertical one becomes the plain move over the buffer lines that skip
/// the closed folds it crosses; any other runs as typed.
fn line_move(c: &Over, cmd: Command, n: u32, m: Motion) -> FoldStep {
    let Some(down) = vertical(m) else {
        return FoldStep::Run(cmd);
    };
    match vertical_to(c, down, n.max(1) as usize) {
        Reach::AsTyped => FoldStep::Run(cmd),
        Reach::Fail => FoldStep::Fail,
        Reach::To(t) if t > c.cur => FoldStep::Run(Command::Move((t - c.cur) as u32, m)),
        Reach::To(t) => FoldStep::Run(Command::Move((c.cur - t) as u32, m)),
    }
}

/// `o`/`O`; a linewise `p`/`P`: placed past the fold. `cmd` is one of them.
fn open_or_put(c: &Over, cmd: Command) -> FoldStep {
    match cmd {
        Command::OpenBelow => c.place(c.end_of(c.cur), cmd),
        Command::OpenAbove => c.place(c.start_of(c.cur), cmd),
        Command::Paste { after, .. } | Command::PasteIndent { after, .. } => {
            let reg = c.ws.registers().get(c.ws.focused().view.pending_register());
            if reg.kind() != RegKind::Linewise || !reg.is_set() {
                return FoldStep::Run(cmd);
            }
            let l = if after {
                c.end_of(c.cur)
            } else {
                c.start_of(c.cur)
            };
            c.place(l, cmd)
        }
        _ => unreachable!("open_or_put takes only o, O, p and P"),
    }
}

/// The count and motion of an operator, `None` for any other command.
fn operand(cmd: &Command) -> Option<(u32, Motion)> {
    Some(match cmd {
        Command::Delete(n, m) | Command::Change(n, m) | Command::Yank(n, m) => (*n, *m),
        // A forced-wise case operator (`gUvj`) runs as typed, like the forced `d`/`c`/`y` (`OpForced`).
        Command::CaseMotion {
            count,
            motion,
            wise: None,
            ..
        }
        | Command::ShiftMotion { count, motion, .. }
        | Command::Reindent { count, motion }
        | Command::Comment { count, motion }
        | Command::Format { count, motion, .. } => (*count, *motion),
        // `>>`/`<<` shift `count` lines; `x`/`<Del>` delete `count` characters (`dl`).
        Command::ShiftRight(n) | Command::ShiftLeft(n) => (*n, Motion::Line),
        Command::DeleteUnder(n) | Command::DeleteForward(n) => (*n, Motion::Right),
        _ => return None,
    })
}

/// An operator over closed folds takes each one it starts or ends in as a whole.
fn operator(c: &Over, cmd: Command) -> FoldStep {
    let Some((count, motion)) = operand(&cmd) else {
        return FoldStep::Run(cmd);
    };
    let count = count.max(1) as usize;
    match operator_span(c, count, motion) {
        Reach::AsTyped => FoldStep::Run(cmd),
        Reach::Fail => FoldStep::Fail,
        Reach::To((first, end)) => {
            let (from, out) = rebuild(cmd, motion, first, end);
            c.place(from, out)
        }
    }
}

/// The lines an operator over `count` and `motion` covers, a closed fold included as a whole:
/// `(first, last)`.
fn operator_span(c: &Over, count: usize, motion: Motion) -> Reach<(usize, usize)> {
    match motion {
        Motion::Line => {
            let plain_end = (c.cur + count - 1).min(c.last);
            if !c.touches(c.cur, plain_end) {
                return Reach::AsTyped;
            }
            let Some(t) = folds::line_down(c.folds, c.cur, count - 1, c.last) else {
                return Reach::Fail;
            };
            Reach::To((c.start_of(c.cur), c.end_of(t)))
        }
        m if vertical(m).is_some() => match vertical_to(c, vertical(m) == Some(true), count) {
            Reach::AsTyped => Reach::AsTyped,
            Reach::Fail => Reach::Fail,
            Reach::To(t) => Reach::To((c.start_of(c.cur.min(t)), c.end_of(c.cur.max(t)))),
        },
        _ => {
            // Any other motion: the lines of its span, when its first line is in a closed fold, or its last
            // line is and it starts at or before the first non-blank (so the span is whole lines there).
            let Some((sl, el)) = c.ws.reindent_range(motion, count as u32) else {
                return Reach::AsTyped;
            };
            let col_ok = {
                let cursor = c.ws.focused().view.cursor().0;
                let ls = c.line_idx.nth_line_start(c.cur);
                c.snapshot[ls..cursor.min(c.snapshot.len())]
                    .iter()
                    .all(|b| matches!(b, b' ' | b'\t'))
            };
            let start_in = folds::closed_range(c.folds, sl).is_some();
            let end_in = folds::closed_range(c.folds, el).is_some();
            if !(start_in || (end_in && (col_ok || linewise(motion)))) {
                return Reach::AsTyped;
            }
            Reach::To((c.start_of(sl), c.end_of(el)))
        }
    }
}

/// The operator `cmd` over `motion` rebuilt to cover lines `first..=end`, with the line it starts from:
/// forward ones start on `first`, a backward `k`/`-` on `end`.
fn rebuild(cmd: Command, motion: Motion, first: usize, end: usize) -> (usize, Command) {
    let lines = (end - first + 1) as u32;
    let gap = (end - first) as u32;
    let (from, n, m) = match motion {
        Motion::Down | Motion::DownFirstNonBlank => (first, gap, motion),
        Motion::Up | Motion::UpFirstNonBlank => (end, gap, motion),
        _ => (first, lines, Motion::Line),
    };
    let out = match cmd {
        Command::Delete(..) => Command::Delete(n, m),
        Command::DeleteUnder(_) | Command::DeleteForward(_) => Command::Delete(lines, Motion::Line),
        // A characterwise change or yank over a closed fold is characterwise over its lines (nvim:
        // `yl` there yanks `L2\nL3\nL4` characterwise): `{n}$` from the first line's start is that span.
        Command::Change(..) if !linewise(motion) => Command::Change(lines, Motion::LineEnd),
        Command::Yank(..) if !linewise(motion) => Command::Yank(lines, Motion::LineEnd),
        Command::Change(..) => Command::Change(n, m),
        Command::Yank(..) => Command::Yank(n, m),
        Command::CaseMotion { case, .. } => Command::CaseMotion {
            count: n,
            motion: m,
            case,
            wise: None,
        },
        Command::ShiftMotion { left, .. } => Command::ShiftMotion {
            left,
            count: n,
            motion: m,
        },
        Command::Reindent { .. } => Command::Reindent {
            count: n,
            motion: m,
        },
        Command::Comment { .. } => Command::Comment {
            count: n,
            motion: m,
        },
        Command::Format { keep_cursor, .. } => Command::Format {
            count: n,
            motion: m,
            keep_cursor,
        },
        Command::ShiftRight(_) => Command::ShiftRight(lines),
        Command::ShiftLeft(_) => Command::ShiftLeft(lines),
        other => other,
    };
    (from, out)
}

/// [`resolve`] `cmd` over the focused buffer's `folds` and do its first step: place the cursor (recorded,
/// so a replay needs no folds) or beep for a failed move. The command to run, or `None` when nothing runs.
pub(crate) fn prepare(
    cmd: Command,
    ws: &mut Workspace,
    folds: &[Fold],
    line_idx: &LineIndex,
    snapshot: &[u8],
    recorded: &mut Vec<Command>,
) -> Option<Command> {
    if !folds.iter().any(|f| f.closed) {
        return Some(cmd);
    }
    match resolve(cmd, ws, folds, line_idx, snapshot) {
        FoldStep::Run(c) => Some(c),
        FoldStep::PlaceThen(at, c) => {
            crate::app::dispatch::place_cursor(ws, recorded, at);
            Some(c)
        }
        FoldStep::Fail => {
            ws.beep();
            None
        }
    }
}

/// Whether a shift, which opens the fold the cursor ends in whatever 'foldopen' holds and for keys not
/// typed too (nvim's `op_shift`).
pub(crate) fn shift_opens_fold(cmd: &Command) -> bool {
    matches!(
        cmd,
        Command::ShiftRight(_) | Command::ShiftLeft(_) | Command::ShiftMotion { .. }
    )
}

/// Whether nvim opens the closed fold the cursor lands in after the typed `cmd`, under the 'foldopen'
/// value `fdo` (#1141; default `block,hor,mark,percent,quickfix,search,tag,undo`): `hor` a horizontal
/// move (`l`, `$`, `w`, `f`…), `percent` `%`, `block` a block move (`}`, `)`, `]]`…), `search` a search
/// or `n`, `mark` a mark or a jump in the jumplist or change list, `undo` undo or redo, `jump` `G` and
/// `gg`, `all` any command. `j`/`k` and `:{n}` are no item. `insert` and `quickfix`/`tag` have no
/// command here: Insert mode opens the fold anyway, and there is no quickfix or tag jump.
pub(crate) fn opens_fold(cmd: &Command, fdo: &str) -> bool {
    use Motion as M;
    let has = |item: &str| fdo.split(',').any(|i| i == item || i == "all");
    if fdo.split(',').any(|i| i == "all") {
        return true;
    }
    match cmd {
        Command::Move(_, m) => match m {
            M::Left
            | M::Right
            | M::WrapLeft
            | M::WrapRight
            | M::LineStart
            | M::LineEnd
            | M::LineFirstNonBlank
            | M::LineLastNonBlank
            | M::Column
            | M::MidLine
            | M::WordFwd
            | M::WordBack
            | M::WordEnd
            | M::WordEndBack
            | M::BigWordFwd
            | M::BigWordBack
            | M::BigWordEnd
            | M::BigWordEndBack
            | M::FindChar { .. } => has("hor"),
            M::MatchBracket => has("percent"),
            M::ParagraphFwd
            | M::ParagraphBack
            | M::SentenceFwd
            | M::SentenceBack
            | M::SectionFwd
            | M::SectionBack
            | M::SectionEndFwd
            | M::SectionEndBack
            | M::UnmatchedParenBack
            | M::UnmatchedParenFwd
            | M::UnmatchedBraceBack
            | M::UnmatchedBraceFwd => has("block"),
            M::GotoLine | M::LastLine => has("jump"),
            _ => false,
        },
        Command::MoveLeft | Command::MoveRight | Command::MoveLineStart | Command::MoveLineEnd => {
            has("hor")
        }
        Command::Search { op, .. } => *op == ruse_core::SearchOp::Move && has("search"),
        Command::SearchNext(_)
        | Command::SearchPrev(_)
        | Command::SearchWordUnder { .. }
        | Command::GotoFirstMatch(_) => has("search"),
        Command::Undo
        | Command::Redo
        | Command::UndoOlder
        | Command::UndoNewer
        | Command::LineUndo => has("undo"),
        Command::GotoNamedMark(_)
        | Command::GotoNamedMarkLine(_)
        | Command::GotoContextMark
        | Command::GotoContextMarkLine
        | Command::GotoChangeMarkStart
        | Command::GotoChangeMarkEnd
        | Command::GotoChangeMarkStartLine
        | Command::GotoChangeMarkEndLine
        | Command::GotoVisualMarkStart
        | Command::GotoVisualMarkEnd
        | Command::GotoVisualMarkStartLine
        | Command::GotoVisualMarkEndLine
        | Command::GotoLastChange
        | Command::GotoLastChangeLine
        | Command::GotoOlderChange
        | Command::GotoNewerChange
        | Command::GotoOlderJump
        | Command::GotoNewerJump => has("mark"),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn foldopen_items_pick_the_commands() {
        let def = crate::input::set_cmd::FOLDOPEN;
        let l = Command::Move(1, Motion::Right);
        let g = Command::Move(3, Motion::GotoLine);
        assert!(opens_fold(&l, def));
        assert!(!opens_fold(&l, ""));
        assert!(!opens_fold(&l, "search,undo"));
        assert!(!opens_fold(&g, def), "`G` is `jump`, not in the default");
        assert!(opens_fold(&g, "jump"));
        assert!(opens_fold(&g, "all"));
        assert!(!opens_fold(
            &Command::Move(1, Motion::Down),
            "hor,block,jump"
        ));
        assert!(opens_fold(&Command::Undo, "undo"));
        assert!(shift_opens_fold(&Command::ShiftRight(1)));
        assert!(crate::input::set_cmd::foldopen_valid("hor,,jump"));
        assert!(!crate::input::set_cmd::foldopen_valid("hor,xyz"));
    }
}
