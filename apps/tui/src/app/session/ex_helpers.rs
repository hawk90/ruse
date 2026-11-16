//! What the Ex executors (`ex_exec`, `ex_line`) share (#1496): the line-count report, `:r`'s text
//! insert, the shell-failure note, the Ex-range cursor move, the backwards-range prompt, the
//! risky-operation test and two error texts. Split from `session.rs`.

use ruse_core::{Command, ExCmd, Workspace};

use crate::app::dispatch::{lines_outcome, run_ex, Files};
use crate::app::messages::Status;
use crate::input::{Ex, ReadSource, WriteTarget};

/// Give a command's line-count report (#1276), or clear the line when it has none ('report' not passed).
pub(super) fn give_report(status: &mut Status, report: Option<String>) {
    match report {
        Some(msg) => status.info(msg),
        None => status.clear(),
    }
}

pub(super) fn to_i64(n: usize) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

/// `:r`: insert `bytes` (what the frontend read from the file or the command) below `addr`, recorded with
/// the bytes as data so a replay never reads or runs anything (RFC-0020 R2). Returns the lines inserted; an
/// address off the buffer inserts nothing.
pub(super) fn read_text(
    ws: &mut Workspace,
    recorded: &mut Vec<Command>,
    addr: ruse_core::LineAddr,
    bytes: Vec<u8>,
    cursor_on_last: bool,
) -> usize {
    ws.resolve_addr(addr).map_or(0, |dest| {
        let ex = ExCmd::ReadText {
            dest,
            cursor_on_last,
            bytes,
        };
        lines_outcome(&run_ex(ex, ws, recorded))
    })
}

/// The status line after a `:r !cmd` / filter: nvim shows `shell returned {n}` ahead of the line count when
/// the command failed (#861).
pub(super) fn with_shell_failure(out: &crate::shell::ShellOutput, lines: String) -> String {
    match (out.failure(), lines.is_empty()) {
        (Some(note), true) => note,
        (Some(note), false) => format!("{note}; {lines}"),
        (None, _) => lines,
    }
}

/// Put the cursor on 1-based `line` for an Ex range: Vim `beginline(BL_SOL | BL_FIX)` under
/// 'nostartofline' keeps the wanted column, so this is `j`/`k` by the distance (nvim v0.12.6 `ll:3` stays
/// in column 2).
pub(super) fn goto_line(
    line: usize,
    ws: &mut Workspace,
    files: &Files,
    recorded: &mut Vec<Command>,
    status: &mut Status,
    quit: &mut bool,
) {
    let b = ws.focused().doc.bytes();
    let here = ruse_core::pos::line_of(b, ws.focused().view.cursor()) + 1;
    let (n, motion) = if line >= here {
        (line - here, ruse_core::Motion::Down)
    } else {
        (here - line, ruse_core::Motion::Up)
    };
    if n > 0 {
        let cmd = Command::Move(n as u32, motion);
        crate::app::dispatch::run_cmd(cmd, ws, files, recorded, status, quit);
    }
}

/// A `:` line stopped at a backwards range (#1093): the command with its range swapped, then the
/// commands after it on the line, run in order once the prompt is answered `y`.
pub(super) struct SwapPrompt {
    pub(super) pending: Vec<String>,
}

/// Whether an Ex command starts another program, a risky operation the journal syncs before (RFC-0023
/// §4): `:!`, a filter, `:r !`, `:w !` and `:terminal`.
pub(super) fn runs_external(ex: &Ex) -> bool {
    matches!(
        ex,
        Ex::Shell(_)
            | Ex::Filter { .. }
            | Ex::Terminal
            | Ex::Read {
                source: ReadSource::Command(_),
                ..
            }
    ) || matches!(ex, Ex::Write(spec) if matches!(spec.target, WriteTarget::Command(_)))
}

pub(super) const E35: &str = "E35: No previous regular expression";
/// `zf` or `:fold` while 'foldmethod' makes the folds (nvim's `e_cant_create_fold_with_current_foldmethod`).
pub(super) const E350: &str = "E350: Cannot create fold with current 'foldmethod'";
