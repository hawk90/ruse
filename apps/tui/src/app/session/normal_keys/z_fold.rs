//! `z` + a fold verb (#1489): [`Session::z_fold`] keeps the one dispatch over the second key, in its
//! original arm order with its guards, and hands each fold family to a helper: open/close, the fold
//! jumps, 'foldlevel', and delete. `zf`, `zv` and the beep fallback stay inline.

use std::collections::HashMap;

use crossterm::event::{KeyCode, KeyEvent};
use ruse_core::{BytePos, Command, DocumentId};

use crate::app::dispatch::place_cursor;
use crate::app::session::{FrameCtx, Prefix, Session, E350};
use crate::folds::Fold;

impl Session {
    /// `z` + a fold verb, or another `z` command (a no-op, or a beep where nvim beeps too).
    pub(super) fn z_fold(&mut self, key: KeyEvent, count: u32, frame: &FrameCtx) {
        let Session {
            ed,
            line_idx,
            prefix,
            folds,
            fold_method,
            foldlevel,
            foldenable,
            ..
        } = self;
        let snapshot = &frame.snapshot;
        // Fold verbs (manual folds; F-007 carve-out). `zf` folds the Visual selection; the rest act on the fold at the cursor.
        let buf = ed.ws.focused_buffer();
        let cur_line = line_idx.line_of(ed.ws.focused().view.cursor().0);
        let fv = folds.entry(buf).or_default();
        match key.code {
            // `zf` — create a CLOSED fold. From a Visual selection: fold the selection's lines. From
            // Normal mode: arm the `zf{motion}` sub-state (handled at the top of the loop), which folds
            // the range the following motion covers (slice 2).
            // Under an indent 'foldmethod' `zf` is `E350` at once and no operator: the keys after it
            // run as typed (nvim's `foldManualAllowed`), and Visual mode stays.
            KeyCode::Char('f')
                if fold_method.get(&buf).copied() == Some(crate::input::FoldMethod::Indent) =>
            {
                ed.status.error(E350);
            }
            KeyCode::Char('f') => {
                if let Some((BytePos(s), BytePos(e))) =
                    ed.ws.focused().view.selection_span(snapshot)
                {
                    let (a, b) = (line_idx.line_of(s), line_idx.line_of(e.saturating_sub(1)));
                    let (start, end) = (a.min(b), a.max(b));
                    ed.run(Command::EnterNormal);
                    crate::folds::insert_closed_fold(fv, start, end);
                    place_cursor(&mut ed.ws, &mut ed.recorded, line_idx.nth_line_start(start));
                    ed.status.echo(format!("folded {} lines", end - start + 1));
                } else {
                    // Normal mode: begin `zf{motion}`. Seed the buffer with any count typed before `z`
                    // (e.g. `3zfj`); further count digits typed after `f` are appended by the handler.
                    *prefix = Prefix::Zf(if count > 0 {
                        count.to_string()
                    } else {
                        String::new()
                    });
                }
            }
            // `zo`/`zc`/`za`, `zO`/`zC`/`zA` ([`open_close`]).
            KeyCode::Char(c @ ('o' | 'c' | 'a' | 'O' | 'C' | 'A')) => {
                if !open_close(fv, cur_line, c, count) {
                    ed.status.error("E490: No fold found");
                }
            }
            // `zj` / `zk` ([`fold_jump`]).
            KeyCode::Char('j') | KeyCode::Char('k') => {
                let down = matches!(key.code, KeyCode::Char('j'));
                if let Some(line) = fold_jump(fv, cur_line, down, count) {
                    place_cursor(&mut ed.ws, &mut ed.recorded, line_idx.nth_line_start(line));
                }
            }
            // `zR`/`zM`/`zr`/`zm`, `zx`/`zX` ([`set_foldlevel`]).
            KeyCode::Char(c @ ('R' | 'M' | 'r' | 'm' | 'x' | 'X')) => {
                set_foldlevel(fv, foldlevel, foldenable, buf, c, count, cur_line);
            }
            // `zd`/`zD`/`zE` ([`delete_folds`]).
            KeyCode::Char(c @ ('d' | 'D' | 'E')) => {
                let indent =
                    fold_method.get(&buf).copied() == Some(crate::input::FoldMethod::Indent);
                if let Some(err) = delete_folds(fv, c, cur_line, indent) {
                    ed.status.error(err);
                }
            }
            // `zv` — view cursor line: open just enough folds to reveal the cursor line. Keeps the
            // folds, unlike `zd`.
            KeyCode::Char('v') => {
                crate::folds::open_cursor_fold(fv, cur_line);
            }
            // A `z` command nvim has but ruse lacks (`zh`, `zs`, `zx`, spelling, …) stays a no-op; one
            // nvim lacks too beeps there (`nv_zet` → `clearopbeep`), stopping a macro or `:normal`
            // (#1111).
            KeyCode::Char(c)
                if key.modifiers.is_empty()
                    && !"abcdefghijklmnoprstuvwxyzACDEFGHLMNOPRWX0123456789+-.^=".contains(c) =>
            {
                ed.ws.beep();
            }
            _ => {}
        }
    }
}

/// `zo`/`zc`/`za`, `zO`/`zC`/`zA` over nested folds (#1141), as nvim's `setManualFold` walks the folds
/// holding the cursor line from the outermost: `zo` opens the outermost closed one (`{count}` levels),
/// `zc` closes the innermost open one above the first closed one, `zO` opens the outermost closed one
/// and all inside it, `zC` closes every one, `za`/`zA` open when a closed fold hides the line and close
/// otherwise. Whether a fold was at the line (none is `E490`). The cursor keeps its line inside a
/// closed fold (nvim; #819).
fn open_close(fv: &mut [Fold], cur_line: usize, c: char, count: u32) -> bool {
    let n = count.max(1);
    let closed = crate::folds::is_closed_at(fv, cur_line);
    let open = match c {
        'o' | 'O' => true,
        'c' | 'C' => false,
        _ => closed,
    };
    let found = match (open, c.is_ascii_uppercase()) {
        (true, true) => crate::folds::open_rec(fv, cur_line),
        (false, true) => crate::folds::close_rec(fv, cur_line),
        (true, false) => (0..n).fold(None, |f, _| f.or(crate::folds::open_one(fv, cur_line))),
        (false, false) => (0..n).fold(None, |f, _| f.or(crate::folds::close_one(fv, cur_line))),
    };
    found.is_some()
}

/// `zj` / `zk` — the line the cursor moves DOWN to (the next fold's start) / UP to (the previous fold's
/// end), `{count}` repeats; `None` when no fold lies that way. A closed fold counts as one: the folds
/// inside it are not stepped to. Cursor-only, column 0 — matching nvim, which ignores 'startofline'
/// here (verified: `zj` lands at col 0 even with `startofline` on). Landing inside a closed fold keeps
/// the cursor there; it paints on the summary row.
fn fold_jump(fv: &[Fold], cur_line: usize, down: bool, count: u32) -> Option<usize> {
    let n = count.max(1);
    let mut line = cur_line;
    let mut moved = false;
    for _ in 0..n {
        let next = if down {
            crate::folds::next_fold_start(fv, line)
        } else {
            crate::folds::prev_fold_end(fv, line)
        };
        match next {
            Some(l) => {
                line = l;
                moved = true;
            }
            None => break, // no more folds that way — keep the last landing
        }
    }
    moved.then_some(line)
}

/// `zR`/`zM`/`zr`/`zm` set 'foldlevel' (the deepest nesting, 0, one more, one less; `{count}` levels
/// for `zr`/`zm`) and `zx`/`zX` re-apply it: a fold deeper than the level is closed, the rest open
/// (nvim's `newFoldLevel`, manual and indent folds alike). `zx` then opens the folds holding the cursor
/// line, as `zv`.
fn set_foldlevel(
    fv: &mut [Fold],
    foldlevel: &mut HashMap<DocumentId, usize>,
    foldenable: &HashMap<DocumentId, bool>,
    buf: DocumentId,
    c: char,
    count: u32,
    cur_line: usize,
) {
    let maxl = crate::folds::max_level(fv);
    let cur = foldlevel.get(&buf).copied().unwrap_or(0);
    let n = count.max(1) as usize;
    let new = match c {
        'R' => maxl,
        'M' => 0,
        'r' => cur.saturating_add(n).min(maxl),
        'm' => cur.saturating_sub(n),
        _ => cur,
    };
    foldlevel.insert(buf, new);
    let en = foldenable.get(&buf).copied().unwrap_or(true);
    crate::folds::apply_foldlevel(fv, if en { new } else { usize::MAX });
    if c == 'x' {
        crate::folds::open_cursor_fold(fv, cur_line);
    }
}

/// `zd` deletes the innermost fold at the cursor line (or the closed one hiding it) and moves the folds
/// inside it up a level; `zD` deletes those too. `zE` deletes every fold. Folds an indent 'foldmethod'
/// makes cannot be deleted (`E351`/`E352`). The error to show, if any.
fn delete_folds(
    fv: &mut Vec<Fold>,
    c: char,
    cur_line: usize,
    indent: bool,
) -> Option<&'static str> {
    if indent {
        Some(if c == 'E' {
            "E352: Cannot erase folds with current 'foldmethod'"
        } else {
            "E351: Cannot delete fold with current 'foldmethod'"
        })
    } else if c == 'E' {
        crate::folds::eliminate_all(fv);
        None
    } else if !crate::folds::delete_at(fv, cur_line, c == 'D') {
        Some("E490: No fold found")
    } else {
        None
    }
}
