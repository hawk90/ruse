//! Full-screen and status-line prompts: the open-time crash-recovery prompt (F-008) and the
//! `:s///c` interactive confirm loop (F-009 #2).

use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::style::Print;
use crossterm::terminal::{self, ClearType};
use crossterm::{cursor, queue};

use ruse_core::{BytePos, Workspace};

/// The user's answer at the open-time recovery prompt (F-008 #3).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum RecoveryChoice {
    /// Load the recovered changes into the buffer.
    Recover,
    /// Delete the journal and open the on-disk file.
    Discard,
    /// Leave without opening anything; the journal stays.
    Quit,
}

/// Map one key press to a recovery choice. Deleting the journal, the only copy of the work, takes an
/// explicit `d`; Esc, `q` and Ctrl-C leave it in place; any other key is ignored.
pub(crate) fn recovery_choice(key: KeyEvent) -> Option<RecoveryChoice> {
    match key.code {
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            Some(RecoveryChoice::Quit)
        }
        KeyCode::Char('r' | 'R' | 'y') => Some(RecoveryChoice::Recover),
        KeyCode::Char('d' | 'D') => Some(RecoveryChoice::Discard),
        KeyCode::Char('q') | KeyCode::Esc => Some(RecoveryChoice::Quit),
        _ => None,
    }
}

/// Set when a full-screen prompt has painted over the editor, so the next frame repaints every cell
/// instead of diffing against a screen the terminal no longer shows.
static SCREEN_CLOBBERED: AtomicBool = AtomicBool::new(false);

/// Whether a prompt painted over the editor since the last call (and reset the flag).
pub(crate) fn take_screen_clobbered() -> bool {
    SCREEN_CLOBBERED.swap(false, Ordering::Relaxed)
}

/// Ask what to do with recovered unsaved changes (F-008 #3). Renders a minimal full-screen prompt and
/// blocks until a key maps to a choice. The original file is never touched here (#4). `quit_label`
/// describes `q`: quitting the editor at startup, cancelling the open for a file opened later (#859). `note`
/// is extra warning lines, e.g. that the file changed on disk after the journal was written (#897). A fatal
/// signal (`crate::signals`) answers [`RecoveryChoice::Quit`], which keeps the journal, so the caller unwinds
/// through the terminal guard instead of dying in raw mode (#897).
pub(crate) fn prompt_recovery(
    out: &mut io::Stdout,
    quit_label: &str,
    note: Option<&str>,
) -> io::Result<RecoveryChoice> {
    SCREEN_CLOBBERED.store(true, Ordering::Relaxed);
    queue!(
        out,
        terminal::Clear(ClearType::All),
        cursor::MoveTo(0, 0),
        Print("ruse: unsaved changes were recovered from a previous session."),
        cursor::MoveTo(0, 1),
        Print(format!(
            "r: recover them   d: delete them and open the on-disk file   {quit_label}"
        )),
    )?;
    for (row, line) in note.unwrap_or_default().lines().enumerate() {
        queue!(out, cursor::MoveTo(0, 2 + row as u16), Print(line))?;
    }
    out.flush()?;
    loop {
        // The signal handler wakes a blocked read with a resize event, so this check runs.
        if crate::signals::pending().is_some() {
            return Ok(RecoveryChoice::Quit);
        }
        // Keys typed ahead (during the startup probe, or while a command ran) answer it first.
        let ev = match crate::breakcheck::pop() {
            Some(ev) => ev,
            None => event::read()?,
        };
        if let Event::Key(k) = ev {
            if k.kind == KeyEventKind::Release {
                continue; // act on the press, not its release (kitty/Windows send both)
            }
            if let Some(choice) = recovery_choice(k) {
                return Ok(choice);
            }
        }
    }
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
