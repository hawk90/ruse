//! Full-screen and status-line prompts: the open-time crash-recovery prompt (F-008) and the
//! `:s///c` interactive confirm loop (F-009 #2).

use std::io::{self, Write};

use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::style::Print;
use crossterm::terminal::{self, ClearType};
use crossterm::{cursor, queue};

use ruse_core::Workspace;

/// Ask the user whether to load recovered unsaved changes (F-008 #3). Renders a minimal full-screen
/// prompt and blocks for one key: `y`/`r` = recover, anything else = discard and open the disk file.
/// The original file is never touched here — the choice only decides the initial BUFFER (#4).
pub(crate) fn prompt_recovery(out: &mut io::Stdout) -> io::Result<bool> {
    queue!(
        out,
        terminal::Clear(ClearType::All),
        cursor::MoveTo(0, 0),
        Print("ruse: unsaved changes were recovered from a previous session."),
        cursor::MoveTo(0, 1),
        Print("Press 'r' to RECOVER them, or any other key to open the on-disk file."),
    )?;
    out.flush()?;
    loop {
        if let Event::Key(k) = event::read()? {
            if k.kind == KeyEventKind::Release {
                continue; // act on the press, not its release (kitty/Windows send both)
            }
            return Ok(matches!(
                k.code,
                KeyCode::Char('r') | KeyCode::Char('y') | KeyCode::Char('R')
            ));
        }
    }
}

/// The state of an in-progress `:s///c` confirm loop (F-009 #2): the pending substitutions, the index
/// of the one being confirmed, and the subset accepted so far. The buffer is NOT edited until the loop
/// ends (all confirmed, or `a`/`l`/`q`), so the absolute offsets stay valid throughout; the accepted
/// subset is then applied as one undo group.
pub(crate) struct Confirm {
    pub(crate) subs: Vec<ruse_core::Substitution>,
    pub(crate) idx: usize,
    accepted: Vec<ruse_core::Substitution>,
}

impl Confirm {
    pub(crate) fn new(subs: Vec<ruse_core::Substitution>) -> Confirm {
        Confirm {
            subs,
            idx: 0,
            accepted: Vec::new(),
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
    status: &mut String,
) {
    let Some(mut c) = confirm.take() else {
        return;
    };
    match key.code {
        KeyCode::Char('y') => {
            if let Some(s) = c.subs.get(c.idx) {
                c.accepted.push(s.clone());
            }
            c.idx += 1;
        }
        KeyCode::Char('n') => c.idx += 1,
        KeyCode::Char('a') => {
            c.accepted.extend(c.subs[c.idx..].iter().cloned());
            c.idx = c.subs.len();
        }
        KeyCode::Char('l') => {
            if let Some(s) = c.subs.get(c.idx) {
                c.accepted.push(s.clone());
            }
            c.idx = c.subs.len();
        }
        KeyCode::Char('q') | KeyCode::Esc => c.idx = c.subs.len(),
        _ => {} // any other key: re-prompt (Vim ignores it)
    }
    if c.idx >= c.subs.len() {
        let out = ws.apply_substitutions(&c.accepted);
        *status = format!("{} substitutions on {} lines", out.replacements, out.lines);
        // `confirm` was taken and stays None — the loop is over.
    } else {
        *confirm = Some(c);
    }
}
