//! How a message too long for the status line is shown (#1222). ruse's last row holds both the status bar
//! and the message after it, so a long message would be cut at the right edge. nvim v0.12.6 (probed in a
//! PTY at 80x24) shows such a message whole: it wraps across as many rows as it needs, the screen scrolls up,
//! and `Press ENTER or type command to continue` waits on the last row (`:help hit-enter`). A file message
//! (`"name" 3L, 12B written`) is cut at its start with a `<` instead, as the default `'shortmess'` flag `t`
//! does; when the bar's own prefix leaves it no room, it is cut to the whole row (#1235).
//!
//! A message taller than the screen (`:messages`, long command output) is paged from its top as nvim's
//! `-- More --` prompt does ([`Pager`], `:help more-prompt`), and its last page ends at the hit-enter prompt.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use unicode_segmentation::UnicodeSegmentation;

use crate::screen::{cluster_width, transcribe};

/// nvim's hit-enter prompt (`:help hit-enter`).
pub(crate) const HIT_ENTER: &str = "Press ENTER or type command to continue";

/// How the status bar shows its message.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Shown {
    /// The bar as it is: the message fits after the bar's prefix.
    Fits,
    /// A file message cut at its start (`<` and its tail), to put after the prefix.
    Cut(String),
    /// A file message cut at its start to the whole row, the bar's prefix left out: the prefix leaves it
    /// no room (#1235). nvim's message row never holds the status line, so it cuts the message to the row.
    CutRow(String),
    /// The message whole, wrapped to the screen width, one entry per row, for the rows above the
    /// hit-enter prompt.
    More(Vec<String>),
}

/// The cells `g` takes on the screen, a control char as its `^X` transcription ([`transcribe`]).
fn cells(g: &str) -> usize {
    match transcribe(g, false) {
        Some(t) => t
            .graphemes(true)
            .map(|c| usize::from(cluster_width(c)))
            .sum(),
        None => usize::from(cluster_width(g)),
    }
}

/// The cells `s` takes on one row.
pub(crate) fn width(s: &str) -> usize {
    s.graphemes(true).map(cells).sum()
}

/// How `message` is shown after a bar prefix `prefix_cells` wide on a screen `cols` wide.
pub(crate) fn fit(prefix_cells: usize, message: &str, cols: usize) -> Shown {
    let room = cols.saturating_sub(prefix_cells);
    if !message.contains('\n') && width(message) <= room {
        return Shown::Fits;
    }
    if is_file_message(message) && !message.contains('\n') {
        if room > 1 {
            return Shown::Cut(cut_start(message, room));
        }
        if width(message) <= cols {
            return Shown::CutRow(message.to_string());
        }
        return Shown::CutRow(cut_start(message, cols));
    }
    Shown::More(wrap(message, cols))
}

/// nvim's `-- More --` prompt (`:help more-prompt`).
pub(crate) const MORE: &str = "-- More --";
/// What nvim adds to it after a key it does not take.
pub(crate) const MORE_HELP: &str = " SPACE/d/j: screen/page/line down, b/u/k: up, q: quit";

/// The prompt the last frame left on the last row: the next key answers it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub(crate) enum Prompt {
    #[default]
    None,
    /// `Press ENTER or type command to continue` (`:help hit-enter`).
    HitEnter,
    /// `-- More --`: the message goes on below the screen (`:help more-prompt`).
    More,
}

/// What a key typed at a prompt does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Answer {
    /// The key moved the message (or was ignored): the prompt stays, the key is used up.
    Stay,
    /// The message is done, the key used up (`<CR>` at the hit-enter prompt, `q` at `-- More --`).
    Done,
    /// The message is done and the key runs as typed (`:` opens the command line).
    DoneThenRun,
}

/// The rows of a message taller than the screen, paged with nvim's `-- More --` keys (#1235), and the
/// prompt the last frame showed. A new message starts at its top.
#[derive(Debug, Default)]
pub(crate) struct Pager {
    /// The message laid out ([`crate::app::messages::Status::serial`]).
    serial: u64,
    /// The first row of the message on the screen.
    top: usize,
    /// The message's rows and how many the screen shows above the prompt, as the last frame laid them out.
    total: usize,
    page: usize,
    /// A key `-- More --` does not take shows its help after it.
    help: bool,
    pub(crate) prompt: Prompt,
}

impl Pager {
    /// Lay out a message of `total` rows over `page` rows: the first row to show, and the prompt.
    pub(crate) fn layout(&mut self, serial: u64, total: usize, page: usize) -> (usize, Prompt) {
        if serial != self.serial {
            self.serial = serial;
            self.top = 0;
            self.help = false;
        }
        self.total = total;
        self.page = page.max(1);
        self.top = self.top.min(self.last_top());
        self.prompt = if self.top < self.last_top() {
            Prompt::More
        } else {
            Prompt::HitEnter
        };
        (self.top, self.prompt)
    }

    /// The text of the last row for `prompt`.
    pub(crate) fn prompt_text(&self, prompt: Prompt) -> String {
        match prompt {
            Prompt::More if self.help => format!("{MORE}{MORE_HELP}"),
            Prompt::More => MORE.to_string(),
            _ => HIT_ENTER.to_string(),
        }
    }

    fn last_top(&self) -> usize {
        self.total.saturating_sub(self.page)
    }

    /// The message was taller than the screen: the hit-enter prompt then scrolls back too.
    fn paged(&self) -> bool {
        self.total > self.page
    }

    fn scroll(&mut self, down: bool, rows: usize) {
        self.top = if down {
            (self.top + rows).min(self.last_top())
        } else {
            self.top.saturating_sub(rows)
        };
    }

    /// A key typed at `-- More --` (`:help more-prompt`): `<CR>`, `<NL>`, `j`, `<Down>` one row on, `d` half
    /// a screen, `<Space>`, `f`, `<PageDown>`, `CTRL-F` a screen, `G` to the end; `<BS>`, `k`, `<Up>` one
    /// row back, `u` half a screen, `b`, `<PageUp>`, `CTRL-B` a screen, `g` to the start; `q`, `<Esc>`,
    /// `CTRL-C` stop, `:` stops and opens the command line. Any other key shows the help.
    pub(crate) fn more_key(&mut self, key: KeyEvent) -> Answer {
        self.help = false;
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let plain = !key
            .modifiers
            .difference(KeyModifiers::SHIFT)
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
        let (page, half) = (self.page, self.page.div_ceil(2));
        match key.code {
            KeyCode::Enter | KeyCode::Down => self.scroll(true, 1),
            KeyCode::Char('j') if plain => self.scroll(true, 1),
            KeyCode::Char('j' | 'm') if ctrl => self.scroll(true, 1),
            KeyCode::Char('d') if plain => self.scroll(true, half),
            KeyCode::Char(' ' | 'f') if plain => self.scroll(true, page),
            KeyCode::PageDown => self.scroll(true, page),
            KeyCode::Char('f') if ctrl => self.scroll(true, page),
            KeyCode::Char('G') if plain => self.top = self.last_top(),
            KeyCode::Backspace | KeyCode::Up => self.scroll(false, 1),
            KeyCode::Char('k') if plain => self.scroll(false, 1),
            KeyCode::Char('u') if plain => self.scroll(false, half),
            KeyCode::Char('b') if plain => self.scroll(false, page),
            KeyCode::PageUp => self.scroll(false, page),
            KeyCode::Char('b') if ctrl => self.scroll(false, page),
            KeyCode::Char('g') if plain => self.top = 0,
            KeyCode::Char('q') if plain => return Answer::Done,
            KeyCode::Esc => return Answer::Done,
            KeyCode::Char('c') if ctrl => return Answer::Done,
            KeyCode::Char(':') if plain => return Answer::DoneThenRun,
            _ => self.help = true,
        }
        Answer::Stay
    }

    /// A key typed at the hit-enter prompt (`:help hit-enter`): `<CR>`, `<NL>`, `<Space>` and `CTRL-C` are
    /// used up, any other key, `<Esc>` too, runs as typed. After a message taller than the screen, `b`,
    /// `k`, `u`, `g`, `<Up>` and `<PageUp>` scroll back into it, and `j`, `d`, `f`, `<Down>` and
    /// `<PageDown>` are ignored (nvim's `wait_return`).
    pub(crate) fn hit_enter_key(&mut self, key: KeyEvent) -> Answer {
        let mods = key.modifiers.difference(KeyModifiers::SHIFT);
        if self.paged() && mods.is_empty() {
            let back = matches!(
                key.code,
                KeyCode::Char('b' | 'k' | 'u' | 'g') | KeyCode::Up | KeyCode::PageUp
            );
            if back {
                return self.more_key(key);
            }
            if matches!(
                key.code,
                KeyCode::Char('j' | 'd' | 'f') | KeyCode::Down | KeyCode::PageDown
            ) {
                return Answer::Stay;
            }
        }
        match key.code {
            KeyCode::Enter | KeyCode::Char(' ') if mods.is_empty() => Answer::Done,
            KeyCode::Char('c' | 'j') if mods == KeyModifiers::CONTROL => Answer::Done,
            _ => Answer::DoneThenRun,
        }
    }
}

/// A file message (`"f.txt" 3L, 12B written`, `"new.txt" [New]`): the quoted name, then a count, a `[`
/// flag or a `--` note. A quoted name followed by anything else (`"f.txt" not opened: …`) is not one.
fn is_file_message(message: &str) -> bool {
    let Some(rest) = message.strip_prefix('"') else {
        return false;
    };
    let Some(end) = rest.find('"') else {
        return false;
    };
    let after = rest[end + 1..].trim_start_matches(' ');
    after.starts_with(|c: char| c.is_ascii_digit() || c == '[') || after.starts_with("--")
}

/// `<` and as much of the end of `message` as fits in `room` cells with it.
fn cut_start(message: &str, room: usize) -> String {
    let mut tail = Vec::new();
    let mut used = 1;
    for g in message.graphemes(true).rev() {
        let w = cells(g);
        if used + w > room {
            break;
        }
        used += w;
        tail.push(g);
    }
    tail.reverse();
    format!("<{}", tail.concat())
}

/// `message` broken into rows of at most `cols` cells, at any cell as nvim does (not at word ends), and at
/// each newline.
fn wrap(message: &str, cols: usize) -> Vec<String> {
    let cols = cols.max(1);
    let mut rows = Vec::new();
    for line in message.split('\n') {
        let mut row = String::new();
        let mut used = 0;
        for g in line.graphemes(true) {
            let w = cells(g);
            if used + w > cols && used > 0 {
                rows.push(std::mem::take(&mut row));
                used = 0;
            }
            row.push_str(g);
            used += w;
        }
        rows.push(row);
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_message_that_fits_is_left_alone() {
        assert_eq!(fit(20, "E37: No write since last change", 80), Shown::Fits);
        assert_eq!(fit(0, &"x".repeat(80), 80), Shown::Fits);
    }

    #[test]
    fn a_long_message_wraps_at_the_screen_width() {
        let msg = format!("E303: {}", "y".repeat(100));
        let Shown::More(rows) = fit(20, &msg, 80) else {
            panic!("not wrapped");
        };
        assert_eq!(rows.len(), 2);
        assert_eq!(width(&rows[0]), 80);
        assert_eq!(rows.concat(), msg);
    }

    #[test]
    fn a_message_too_long_after_the_prefix_waits_for_enter_too() {
        // 60 cells fit the screen but not after a 30-cell prefix.
        let msg = "W".repeat(60);
        assert_eq!(fit(30, &msg, 80), Shown::More(vec![msg.clone()]));
    }

    #[test]
    fn a_newline_starts_a_row() {
        assert_eq!(
            fit(0, "one\ntwo", 80),
            Shown::More(vec!["one".into(), "two".into()])
        );
    }

    #[test]
    fn a_file_message_is_cut_at_its_start() {
        let msg = format!("\"{}/f.txt\" 3L, 12B written", "d".repeat(80));
        let Shown::Cut(shown) = fit(20, &msg, 80) else {
            panic!("not cut");
        };
        assert_eq!(width(&shown), 60);
        assert!(shown.starts_with("<ddd"), "{shown}");
        assert!(shown.ends_with("/f.txt\" 3L, 12B written"), "{shown}");
    }

    #[test]
    fn only_a_file_message_is_cut() {
        assert!(is_file_message("\"f.txt\" 3L, 12B written"));
        assert!(is_file_message("\"f.txt\" [New]"));
        assert!(is_file_message("\"f.txt\" --No lines in buffer--"));
        let refused = format!(
            "\"f.txt\" not opened: recovery journal kept ({})",
            "j".repeat(80)
        );
        assert!(!is_file_message(&refused));
        assert!(matches!(fit(20, &refused, 80), Shown::More(_)));
    }

    #[test]
    fn a_file_message_after_a_too_wide_prefix_is_cut_to_the_row() {
        let msg = format!("\"{}/f.txt\" 3L, 12B written", "d".repeat(80));
        let Shown::CutRow(shown) = fit(85, &msg, 80) else {
            panic!("not cut to the row");
        };
        assert_eq!(width(&shown), 80);
        assert!(shown.starts_with("<ddd"), "{shown}");
        assert_eq!(
            fit(85, "\"f.txt\" [New]", 80),
            Shown::CutRow("\"f.txt\" [New]".into())
        );
    }

    fn key(c: KeyCode) -> KeyEvent {
        KeyEvent::new(c, KeyModifiers::NONE)
    }

    /// nvim v0.12.6's `-- More --` over 50 rows on an 80x24 screen (23 rows a page), probed in a PTY.
    #[test]
    fn more_keys_page_as_nvim_does() {
        let mut p = Pager::default();
        let top = |p: &mut Pager| p.layout(1, 50, 23);
        assert_eq!(top(&mut p), (0, Prompt::More));
        let steps = [
            (KeyCode::Char(' '), 23),
            (KeyCode::Char('j'), 24),
            (KeyCode::Enter, 25),
            (KeyCode::Char('b'), 2),
            (KeyCode::Char('k'), 1),
            (KeyCode::Char('d'), 13),
            (KeyCode::Char('u'), 1),
            (KeyCode::Char('g'), 0),
            (KeyCode::Char('G'), 27),
        ];
        for (k, want) in steps {
            assert_eq!(p.more_key(key(k)), Answer::Stay, "{k:?}");
            assert_eq!(top(&mut p).0, want, "after {k:?}");
        }
        // The end: the hit-enter prompt, where `b`/`k`/`u`/`g` go back and `j`/`f`/`d` do nothing.
        assert_eq!(top(&mut p).1, Prompt::HitEnter);
        assert_eq!(p.hit_enter_key(key(KeyCode::Char('j'))), Answer::Stay);
        assert_eq!(top(&mut p), (27, Prompt::HitEnter));
        assert_eq!(p.hit_enter_key(key(KeyCode::Char('u'))), Answer::Stay);
        assert_eq!(top(&mut p), (15, Prompt::More));
        // A key it does not take shows the help; `q` and `<Esc>` end it, `:` ends it and runs.
        assert_eq!(p.more_key(key(KeyCode::Char('x'))), Answer::Stay);
        assert_eq!(p.prompt_text(Prompt::More), format!("{MORE}{MORE_HELP}"));
        assert_eq!(p.more_key(key(KeyCode::Char('q'))), Answer::Done);
        assert_eq!(p.more_key(key(KeyCode::Esc)), Answer::Done);
        assert_eq!(p.more_key(key(KeyCode::Char(':'))), Answer::DoneThenRun);
        // A new message starts at its top.
        assert_eq!(p.layout(2, 50, 23), (0, Prompt::More));
    }

    #[test]
    fn a_message_that_fits_the_screen_asks_for_enter_only() {
        let mut p = Pager::default();
        assert_eq!(p.layout(1, 5, 23), (0, Prompt::HitEnter));
        assert_eq!(
            p.hit_enter_key(key(KeyCode::Char('k'))),
            Answer::DoneThenRun
        );
        assert_eq!(p.hit_enter_key(key(KeyCode::Enter)), Answer::Done);
    }

    #[test]
    fn wide_chars_do_not_split_across_rows() {
        let Shown::More(rows) = fit(0, &"界".repeat(41), 81) else {
            panic!("not wrapped");
        };
        assert_eq!(rows.iter().map(|r| width(r)).collect::<Vec<_>>(), [80, 2]);
    }
}
