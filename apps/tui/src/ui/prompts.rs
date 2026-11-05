//! Status-line prompts: the `:s///c` interactive confirm loop (F-009 #2), and the flag a full-screen prompt
//! (the open-time recovery prompt, [`super::recovery`]) sets when it paints over the editor.

use std::sync::atomic::{AtomicBool, Ordering};

use crossterm::event::KeyCode;

use ruse_core::{BytePos, Workspace};

/// Set when a full-screen prompt has painted over the editor, so the next frame repaints every cell
/// instead of diffing against a screen the terminal no longer shows.
static SCREEN_CLOBBERED: AtomicBool = AtomicBool::new(false);

/// Whether a prompt painted over the editor since the last call (and reset the flag).
pub(crate) fn take_screen_clobbered() -> bool {
    SCREEN_CLOBBERED.swap(false, Ordering::Relaxed)
}

/// Note that a full-screen prompt painted over the editor ([`take_screen_clobbered`]).
pub(crate) fn mark_screen_clobbered() {
    SCREEN_CLOBBERED.store(true, Ordering::Relaxed);
}

/// The state of an in-progress `:s///c` confirm loop (F-009 #2): the pending substitutions, the index
/// of the one being confirmed, and the answers given so far. The buffer is NOT edited until the loop
/// ends (all confirmed, or `a`/`l`/`q`), so the absolute offsets stay valid throughout; the accepted
/// subset is then applied as one undo group, through the resolved `:s` recorded with the answers
/// (RFC-0020), so a trace replays the same choice without a prompt.
pub(crate) struct Confirm {
    pub(crate) subs: Vec<ruse_core::Substitution>,
    pub(crate) idx: usize,
    answers: Vec<bool>,
    /// The resolved `:s` (its `answers` filled in when the loop ends).
    ex: ruse_core::ExCmd,
    /// The cursor when the loop began (the frame follows the current match meanwhile).
    from: usize,
}

impl Confirm {
    pub(crate) fn new(
        subs: Vec<ruse_core::Substitution>,
        ex: ruse_core::ExCmd,
        from: usize,
    ) -> Confirm {
        Confirm {
            subs,
            idx: 0,
            answers: Vec::new(),
            ex,
            from,
        }
    }
}

/// The status-line prompt shown while confirming the current match.
pub(crate) fn confirm_prompt(c: &Confirm) -> String {
    let rep = c
        .subs
        .get(c.idx)
        .map(|s| String::from_utf8_lossy(&s.replacement).into_owned())
        .unwrap_or_default();
    format!(
        "replace with {rep} ({}/{})?  (y)es (n)o (a)ll (l)ast (q)uit",
        c.idx + 1,
        c.subs.len()
    )
}

/// Handle one key of the `:s///c` confirm loop. Applies the accepted subset (one undo group) and clears
/// `confirm` when the loop ends; otherwise advances to the next match.
pub(crate) fn confirm_key(
    confirm: &mut Option<Confirm>,
    key: crossterm::event::KeyEvent,
    ws: &mut Workspace,
    recorded: &mut Vec<ruse_core::Command>,
    status: &mut String,
) {
    let Some(mut c) = confirm.take() else {
        return;
    };
    let n = c.subs.len();
    match key.code {
        KeyCode::Char('y') => {
            c.answers.push(true);
            c.idx += 1;
        }
        KeyCode::Char('n') => {
            c.answers.push(false);
            c.idx += 1;
        }
        KeyCode::Char('a') => {
            c.answers.resize(n, true);
            c.idx = n;
        }
        KeyCode::Char('l') => {
            c.answers.push(true);
            c.idx = n;
        }
        KeyCode::Char('q') | KeyCode::Esc => c.idx = n,
        _ => {} // any other key: re-prompt (Vim ignores it)
    }
    if c.idx >= n {
        c.answers.resize(n, false);
        let Confirm {
            mut ex,
            answers,
            from,
            ..
        } = c;
        if let ruse_core::ExCmd::Substitute { answers: a, .. } = &mut ex {
            *a = Some(answers);
        }
        // Run it from where the loop began (the jump it records is that position), then leave the
        // cursor on the last match shown when nothing was replaced, as the loop did live.
        let shown = ws.focused().view.cursor().0;
        ws.place_focused_cursor(BytePos(from));
        let out = crate::app::dispatch::sub_outcome(crate::app::dispatch::run_ex(ex, ws, recorded))
            .unwrap_or(ruse_core::SubOutcome {
                replacements: 0,
                lines: 0,
            });
        if out.replacements == 0 {
            crate::app::dispatch::place_cursor(ws, recorded, shown);
        }
        *status = crate::app::dispatch::substituted_message(&out);
        // `confirm` was taken and stays None — the loop is over.
    } else {
        *confirm = Some(c);
    }
}
