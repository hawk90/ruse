//! The open-time recovery prompt (F-008, RFC-0023 guide-level section and §7). One prompt serves the startup
//! file and every file opened later (`:e`, the file picker): [`resolve`] shows it, acts on the answer, and
//! says what to open.
//!
//! * A journal of unsaved work from a session that is gone: `[r] recover   [d] show diff   [e] edit the
//!   disk file   [k] discard (archived)   [q] quit`. `d` shows a unified diff (disk against recovered) in a
//!   read-only full-screen view and then asks again; `e` and `k` move the journal to the archive, never
//!   delete it; `q` and `Esc` keep it where it is.
//! * A journal whose owner is a ruse still running on this host (Neovim's E325): `[o] open read-only   [e]
//!   edit anyway   [q] quit`. Read-only means `:w` refuses with E45 unless forced.
//!
//! No other key does anything (#846): a stray key can neither discard work nor dismiss the prompt. Ctrl-C
//! answers `q`, which keeps everything. Recovery never writes the original file; only `:w` does.

use std::io::{self, Write};
use std::path::{Path, PathBuf};

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::style::Print;
use crossterm::terminal::{self, ClearType};
use crossterm::{cursor, queue};

use crate::persist::encoding::FileFormat;
use crate::persist::journal::{self, clock, date_time};
use crate::persist::rj01::Owner;
use crate::persist::{diff, Available, Recovery};

/// An answer to the recovery prompt.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Choice {
    /// `r`: open the recovered text as an unsaved change over the disk text.
    Recover,
    /// `d`: show the diff, then ask again.
    Diff,
    /// `e`: open the disk file; the journal goes to the archive (the design doc's `KeepInMemory`).
    EditDisk,
    /// `k`: open the disk file; the journal goes to the archive (`KeepDiskFile`).
    Discard,
    /// `q`, `Esc`, Ctrl-C: open nothing; the journal stays.
    Quit,
}

/// An answer to the in-use prompt.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum InUseChoice {
    /// `o`: open the disk file read-only.
    ReadOnly,
    /// `e`: open it for editing anyway; this session journals beside the other's.
    EditAnyway,
    /// `q`, `Esc`, Ctrl-C: open nothing.
    Quit,
}

fn ctrl_c(key: &KeyEvent) -> bool {
    key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL)
}

fn plain(key: &KeyEvent) -> Option<char> {
    match key.code {
        KeyCode::Char(c)
            if !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
        {
            Some(c)
        }
        _ => None,
    }
}

/// The recovery prompt's answer for one key; `None` for every key that is not a choice.
pub(crate) fn choice(key: KeyEvent) -> Option<Choice> {
    if ctrl_c(&key) || key.code == KeyCode::Esc {
        return Some(Choice::Quit);
    }
    match plain(&key)? {
        'r' => Some(Choice::Recover),
        'd' => Some(Choice::Diff),
        'e' => Some(Choice::EditDisk),
        'k' => Some(Choice::Discard),
        'q' => Some(Choice::Quit),
        _ => None,
    }
}

/// The in-use prompt's answer for one key; `None` for every key that is not a choice.
pub(crate) fn in_use_choice(key: KeyEvent) -> Option<InUseChoice> {
    if ctrl_c(&key) || key.code == KeyCode::Esc {
        return Some(InUseChoice::Quit);
    }
    match plain(&key)? {
        'o' => Some(InUseChoice::ReadOnly),
        'e' => Some(InUseChoice::EditAnyway),
        'q' => Some(InUseChoice::Quit),
        _ => None,
    }
}

/// `s` with every control character shown as `^X` (`^?` for DEL) and C1 controls as `?`: a file name, host
/// name or diff line from a journal never reaches the terminal as an escape sequence.
pub(crate) fn printable(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\t' => out.push_str("    "),
            '\u{7f}' => out.push_str("^?"),
            c if (c as u32) < 0x20 => {
                out.push('^');
                out.push(char::from(b'@' + c as u8));
            }
            c if ('\u{80}'..='\u{9f}').contains(&c) => out.push('?'),
            c => out.push(c),
        }
    }
    out
}

fn who(o: &Owner) -> String {
    format!(
        "ruse pid {} on {}",
        o.pid,
        printable(&String::from_utf8_lossy(&o.host))
    )
}

/// The recovery prompt's lines. `quit` names what `q` does (`quit` at startup, `cancel` for a later open).
pub(crate) fn lines(name: &str, a: &Available, quit: &str) -> Vec<String> {
    let name = printable(name);
    let owner = a
        .owner
        .as_ref()
        .map_or_else(|| "an older ruse".to_string(), who);
    let when = a
        .last_change
        .or(a.up_to)
        .map(|t| format!(", last change {}", date_time(t)))
        .unwrap_or_default();
    let mut out = vec![format!("Recovery journal for {name} ({owner}{when})")];
    out.push(match a.changes {
        None => "  unsaved changes from an older ruse's snapshot journal.".to_string(),
        Some(1) => "  1 change after the last save.".to_string(),
        Some(n) if n > 1 => format!("  {n} changes after the last save."),
        Some(_) => "  unsaved changes after the last save.".to_string(),
    });
    if let Some(t) = a.stale {
        out.push(format!(
            "  note: the file on disk is NEWER (modified {}); recovering replaces it in the buffer",
            clock(t)
        ));
    }
    if a.lost_tail {
        out.push(match a.up_to {
            Some(t) => format!(
                "  note: recovered up to {}; later changes could not be recovered",
                clock(t)
            ),
            None => {
                "  note: the end of the journal was damaged; later changes could not be recovered"
                    .to_string()
            }
        });
    }
    out.push(format!(
        "[r] recover   [d] show diff   [e] edit the disk file   [k] discard (archived)   [q] {quit}"
    ));
    out
}

/// The in-use prompt's lines (Neovim's E325 for a swap file whose process is still running).
pub(crate) fn in_use_lines(name: &str, o: &Owner, quit: &str) -> Vec<String> {
    vec![
        format!(
            "E325: ATTENTION: {} is being edited by {} (started {}, still running)",
            printable(name),
            who(o),
            date_time(o.start_ms)
        ),
        "  Another ruse may be editing the same file. Changes made in both are not merged: quit, or \
         continue with caution."
            .to_string(),
        format!("[o] open read-only   [e] edit anyway   [q] {quit}"),
    ]
}

/// What to open after the prompt.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) enum Resolved {
    /// Open the file: `recovered` text over the disk text when the user recovered, `read_only` when they
    /// chose to open it read-only, and `note` for the status line.
    Open {
        recovered: Option<Vec<u8>>,
        read_only: bool,
        note: Option<String>,
    },
    /// Open nothing; `journal` stays where it is.
    Quit { journal: PathBuf },
}

/// Show the prompt `rec` needs (none for [`Recovery::None`] or [`Recovery::Unreplayable`]) and act on the
/// answer. `name` is the file as the user named it, `doc` its path (`None` for the unnamed start buffer),
/// `disk` its decoded text and `fmt` its format. A fatal signal (`crate::signals`) answers quit, so the
/// caller unwinds through the terminal guard instead of dying in raw mode (#897).
pub(crate) fn resolve(
    out: &mut io::Stdout,
    rec: Recovery,
    name: &str,
    doc: Option<&Path>,
    disk: &[u8],
    fmt: FileFormat,
    quit: &str,
) -> io::Result<Resolved> {
    let open = |recovered, read_only, note| Resolved::Open {
        recovered,
        read_only,
        note,
    };
    match rec {
        Recovery::None => Ok(open(None, false, None)),
        Recovery::Unreplayable { reason } => Ok(open(None, false, Some(reason))),
        Recovery::InUse { owner, journal } => {
            let lines = in_use_lines(name, &owner, quit);
            let also = format!("also open in {}", who(&owner));
            Ok(match ask(out, &lines, in_use_choice)? {
                Some(InUseChoice::ReadOnly) => {
                    open(None, true, Some(format!("[readonly]: {also}")))
                }
                Some(InUseChoice::EditAnyway) => {
                    open(None, false, Some(format!("editing anyway: {also}")))
                }
                Some(InUseChoice::Quit) | None => Resolved::Quit { journal },
            })
        }
        Recovery::Available(a) => {
            let lines = lines(name, &a, quit);
            loop {
                let pick = ask(out, &lines, choice)?;
                let Some(j) = journal::current() else {
                    return Ok(Resolved::Quit {
                        journal: a.journal.clone(),
                    });
                };
                return Ok(match pick {
                    Some(Choice::Diff) => {
                        let d = diff::unified(name, disk, &a.recovered);
                        if !show_diff(out, &d)? {
                            return Ok(Resolved::Quit {
                                journal: a.journal.clone(),
                            });
                        }
                        continue;
                    }
                    Some(Choice::Recover) => {
                        let note = match j.adopt(&a, doc, fmt) {
                            Ok(_) => "recovered unsaved changes (:w to save)".to_string(),
                            Err(e) => format!(
                                "recovered unsaved changes (:w to save); the journal was not taken \
                                 over ({e}): {}",
                                a.journal.display()
                            ),
                        };
                        open(Some(a.recovered.clone()), false, Some(note))
                    }
                    Some(c @ (Choice::EditDisk | Choice::Discard)) => {
                        let what = if c == Choice::EditDisk {
                            "opened the disk file"
                        } else {
                            "recovery discarded"
                        };
                        let note = match j.archive(&a.journal) {
                            Ok(p) => format!("{what}; journal archived as {}", p.display()),
                            Err(e) => format!(
                                "{what}; the journal could not be archived ({e}) and stays: {}",
                                a.journal.display()
                            ),
                        };
                        open(None, false, Some(note))
                    }
                    Some(Choice::Quit) | None => Resolved::Quit {
                        journal: a.journal.clone(),
                    },
                });
            }
        }
    }
}

/// The next key press, typed-ahead keys first (the startup probe, or a command that was running); `None`
/// when a fatal signal is pending.
fn next_key() -> io::Result<Option<Option<KeyEvent>>> {
    // The signal handler wakes a blocked read with a resize event, so this check runs.
    if crate::signals::pending().is_some() {
        return Ok(None);
    }
    let ev = match crate::breakcheck::pop() {
        Some(ev) => ev,
        None => event::read()?,
    };
    Ok(Some(match ev {
        // Act on the press, not its release (kitty/Windows send both).
        Event::Key(k) if k.kind != KeyEventKind::Release => Some(k),
        _ => None,
    }))
}

/// Paint `lines` full screen and wait for a key `map` accepts; `None` for a fatal signal. A resize repaints.
fn ask<T>(
    out: &mut io::Stdout,
    lines: &[String],
    map: impl Fn(KeyEvent) -> Option<T>,
) -> io::Result<Option<T>> {
    crate::ui::prompts::mark_screen_clobbered();
    paint(out, lines)?;
    loop {
        match next_key()? {
            None => return Ok(None),
            Some(Some(k)) => {
                if let Some(t) = map(k) {
                    return Ok(Some(t));
                }
            }
            Some(None) => paint(out, lines)?,
        }
    }
}

fn paint(out: &mut io::Stdout, lines: &[String]) -> io::Result<()> {
    let (cols, _) = terminal::size().unwrap_or((80, 24));
    queue!(out, terminal::Clear(ClearType::All))?;
    let rows = lines.iter().flat_map(|l| wrap(l, usize::from(cols)));
    for (row, line) in rows.enumerate() {
        queue!(out, cursor::MoveTo(0, row as u16), Print(line))?;
    }
    out.flush()
}

/// `s` broken into rows of at most `cols` display columns, at a space where there is one (the key line
/// is wider than 80 columns).
pub(crate) fn wrap(s: &str, cols: usize) -> Vec<String> {
    use unicode_width::UnicodeWidthChar;
    let cols = cols.max(1);
    let mut rows = Vec::new();
    let mut rest = s;
    loop {
        let mut w = 0;
        let mut end = rest.len();
        let mut last_space = None;
        for (i, c) in rest.char_indices() {
            w += c.width().unwrap_or(0);
            if w > cols {
                end = i;
                break;
            }
            if c == ' ' {
                last_space = Some(i);
            }
        }
        if end == rest.len() {
            rows.push(rest.to_string());
            return rows;
        }
        let cut = last_space.filter(|&i| i > 0).unwrap_or(end).max(
            rest.chars().next().map_or(1, char::len_utf8), // always make progress
        );
        rows.push(rest[..cut].trim_end().to_string());
        rest = rest[cut..].trim_start();
        if rest.is_empty() {
            return rows;
        }
    }
}

/// `s` cut to `cols` display columns.
fn clip(s: &str, cols: usize) -> String {
    use unicode_width::UnicodeWidthChar;
    let mut w = 0;
    s.chars()
        .take_while(|c| {
            w += c.width().unwrap_or(0);
            w <= cols
        })
        .collect()
}

/// The diff view's bottom line.
fn diff_status(d: &diff::Diff, top: usize, rows: usize, total: usize) -> String {
    let last = (top + rows).min(total);
    let mut s = format!(
        "-- recovery diff, lines {}-{last} of {total} --  j/k: scroll  Space/b: page  g/G: top/end  q: back",
        top + 1
    );
    if !d.minimal {
        s.push_str("  (too large to compare in full: the changed block is shown whole)");
    }
    s
}

/// The `d` view: the diff read-only, full screen, scrolled with `j`/`k`/arrows, `Space`/`b`/PageDown/PageUp
/// and `g`/`G`; `q` or `Esc` goes back to the prompt. `false` for a fatal signal.
fn show_diff(out: &mut io::Stdout, d: &diff::Diff) -> io::Result<bool> {
    let mut body: Vec<String> = d.lines.iter().map(|l| printable(l)).collect();
    if d.cut > 0 {
        body.push(format!(
            "... {} more lines not shown (the diff is cut at {} lines)",
            d.cut,
            diff::MAX_OUTPUT
        ));
    }
    let mut top = 0usize;
    loop {
        let (cols, term_rows) = terminal::size().unwrap_or((80, 24));
        let rows = usize::from(term_rows.max(2)) - 1;
        let max_top = body.len().saturating_sub(rows);
        top = top.min(max_top);
        crate::ui::prompts::mark_screen_clobbered();
        queue!(out, terminal::Clear(ClearType::All))?;
        for (row, line) in body.iter().skip(top).take(rows).enumerate() {
            queue!(
                out,
                cursor::MoveTo(0, row as u16),
                Print(clip(line, cols as usize))
            )?;
        }
        queue!(
            out,
            cursor::MoveTo(0, rows as u16),
            Print(clip(&diff_status(d, top, rows, body.len()), cols as usize))
        )?;
        out.flush()?;
        let k = match next_key()? {
            None => return Ok(false),
            Some(Some(k)) => k,
            Some(None) => continue, // a resize: repaint
        };
        if ctrl_c(&k) || k.code == KeyCode::Esc {
            return Ok(true);
        }
        let page = rows.saturating_sub(1).max(1);
        match (k.code, plain(&k)) {
            (_, Some('q')) => return Ok(true),
            (KeyCode::Down, _) | (_, Some('j')) | (KeyCode::Enter, _) => top += 1,
            (KeyCode::Up, _) | (_, Some('k')) => top = top.saturating_sub(1),
            (KeyCode::PageDown, _) | (_, Some(' ' | 'f')) => top += page,
            (KeyCode::PageUp, _) | (_, Some('b')) => top = top.saturating_sub(page),
            (KeyCode::Home, _) | (_, Some('g')) => top = 0,
            (KeyCode::End, _) | (_, Some('G')) => top = max_top,
            _ => {}
        }
        top = top.min(max_top);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }

    #[test]
    fn only_the_listed_keys_answer() {
        assert_eq!(choice(key('r')), Some(Choice::Recover));
        assert_eq!(choice(key('d')), Some(Choice::Diff));
        assert_eq!(choice(key('e')), Some(Choice::EditDisk));
        assert_eq!(choice(key('k')), Some(Choice::Discard));
        assert_eq!(choice(key('q')), Some(Choice::Quit));
        assert_eq!(
            choice(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            Some(Choice::Quit)
        );
        assert_eq!(
            choice(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Some(Choice::Quit)
        );
        // #846: everything else, the old `y` and `D` among them, does nothing.
        for c in ['y', 'n', 'D', 'R', 'x', ' ', 'o'] {
            assert_eq!(choice(key(c)), None, "{c:?}");
        }
        assert_eq!(
            choice(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            None
        );
        assert_eq!(
            choice(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL)),
            None
        );
        assert_eq!(in_use_choice(key('o')), Some(InUseChoice::ReadOnly));
        assert_eq!(in_use_choice(key('e')), Some(InUseChoice::EditAnyway));
        assert_eq!(in_use_choice(key('q')), Some(InUseChoice::Quit));
        for c in ['r', 'd', 'k', 'x'] {
            assert_eq!(in_use_choice(key(c)), None, "{c:?}");
        }
    }

    fn offer() -> Available {
        Available {
            recovered: b"work\n".to_vec(),
            base: None,
            changes: Some(37),
            last_change: Some(1_000),
            up_to: Some(2_000),
            lost_tail: false,
            stale: None,
            owner: Some(Owner {
                pid: 4182,
                host: b"laptop".to_vec(),
                start_ms: 0,
            }),
            journal: PathBuf::from("/s/j.rj"),
            legacy: false,
        }
    }

    #[test]
    fn the_prompt_reads_as_the_rfc_shows_it() {
        let l = lines("notes.md", &offer(), "quit");
        assert!(
            l[0].starts_with(
                "Recovery journal for notes.md (ruse pid 4182 on laptop, last change "
            ),
            "{}",
            l[0]
        );
        assert_eq!(l[1], "  37 changes after the last save.");
        assert_eq!(
            l.last().unwrap(),
            "[r] recover   [d] show diff   [e] edit the disk file   [k] discard (archived)   [q] quit"
        );
        assert_eq!(l.len(), 3);
        let mut a = offer();
        a.stale = Some(3_000);
        a.lost_tail = true;
        let l = lines("notes.md", &a, "cancel");
        assert!(
            l[2].contains("the file on disk is NEWER (modified "),
            "{}",
            l[2]
        );
        assert!(
            l[3].starts_with("  note: recovered up to ")
                && l[3].ends_with("; later changes could not be recovered"),
            "{}",
            l[3]
        );
        assert!(l[4].ends_with("[q] cancel"));
        let i = in_use_lines("notes.md", a.owner.as_ref().unwrap(), "quit");
        assert!(
            i[0].starts_with(
                "E325: ATTENTION: notes.md is being edited by ruse pid 4182 on laptop"
            ),
            "{}",
            i[0]
        );
        assert_eq!(i[2], "[o] open read-only   [e] edit anyway   [q] quit");
    }

    #[test]
    fn long_lines_wrap_at_spaces() {
        let keys = lines("f", &offer(), "quit").pop().unwrap();
        let rows = wrap(&keys, 80);
        assert_eq!(
            rows,
            [
                "[r] recover   [d] show diff   [e] edit the disk file   [k] discard (archived)",
                "[q] quit"
            ]
        );
        assert_eq!(wrap("abcdef", 4), ["abcd", "ef"]);
        assert_eq!(wrap("short", 80), ["short"]);
        assert_eq!(wrap("", 80), [""]);
        assert_eq!(
            wrap("界界界", 1),
            ["界", "界", "界"],
            "a wide character still advances"
        );
    }

    #[test]
    fn journal_text_never_reaches_the_terminal_as_control_bytes() {
        assert_eq!(printable("a\u{1b}[2Jb\u{7f}\u{9b}"), "a^[[2Jb^??");
        let mut a = offer();
        a.owner.as_mut().unwrap().host = b"h\x1b]0;x\x07".to_vec();
        let l = lines("f\u{1b}.txt", &a, "quit");
        assert!(l.iter().all(|l| !l.chars().any(char::is_control)), "{l:?}");
    }
}
