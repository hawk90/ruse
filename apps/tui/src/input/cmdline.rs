//! The command-line namespace's owned state (F-026) and its line editor (`:help cmdline-editing`). The
//! `InputEngine` methods that route keys here (`open_cmdline`, `open_minibuffer`, `cmdline`,
//! `feed_cmdline`, `submit_search`) stay in the engine core; the editing primitives — caret motion,
//! deletion, overstrike, the `c_CTRL-V` literal and `c_CTRL-K` digraph collectors — live on [`CmdLine`],
//! which owns no history ring and knows nothing about registers or documents.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// The command-line namespace's owned state (F-026 acceptance #2): a prefix, a line buffer, and a
/// cursor. `ex_mode` distinguishes the `gQ` Ex namespace (stays open, re-prompting after each `<CR>`)
/// from a one-shot `:`/`/` line.
pub(crate) struct CmdLine {
    /// `:` (ex) or `/` (search) — also the glyph the status line shows.
    pub(crate) prefix: char,
    /// The text typed so far. Owned HERE, never on the frontend.
    pub(crate) buffer: String,
    /// The caret as a CHAR index into `buffer` (`0..=len`): typing inserts here, `<BS>` deletes before it.
    pub(crate) cursor: usize,
    /// `gQ` Ex mode: `<CR>` executes AND re-opens the line; `:visual`/`:vi`/empty exits.
    pub(crate) ex_mode: bool,
    /// Emacs `M-x` minibuffer (F-012): the buffer holds a COMMAND NAME (not an ex line), resolved on `<CR>`
    /// against the command registry into a [`ruse_core::Command`]. `false` for the Vim `:`/`/` line.
    pub(crate) mx: bool,
    /// Expression-register prompt (`:help quote=` / `:help i_CTRL-R`): when `Some`, the buffer holds an
    /// EXPRESSION (not an ex line or search pattern), and `<CR>` hands it to the evaluator. The target says
    /// what to do with the result — arm the `"=` register for the next paste, or insert it at the caret.
    /// `None` for the ordinary `:`/`/` line and the `M-x` minibuffer.
    pub(crate) expr: Option<ExprTarget>,
    /// `c_CTRL-R` prefix armed: the NEXT key names the register (or `CTRL-W`/`CTRL-A`/`CTRL-L` the text
    /// under the cursor) to splice at the caret. The frontend resolves the text (the engine has no
    /// registers or document). Reset per key.
    pub(crate) ctrl_r: bool,
    /// The transient history-recall cursor + saved draft for THIS open prompt (`:help cmdline-history`).
    /// Fresh per open; the persistent rings live on the engine (`ex_history`/`search_history`). Dropped
    /// with the prompt — KL-OBL-4 (a layer owns its state and dies with it).
    pub(crate) walk: super::history::HistWalk,
    /// `c_<Insert>` toggled overstrike: typed chars replace the char under the caret instead of inserting.
    pub(crate) overstrike: bool,
    /// `c_CTRL-V` / `c_CTRL-Q` literal entry in flight (`:help c_CTRL-V`).
    pub(crate) literal: Option<Literal>,
    /// `c_CTRL-K` digraph entry in flight: `Some(None)` awaits the first char, `Some(Some(c1))` the second.
    pub(crate) digraph: Option<Option<char>>,
    /// The wildmenu (`:help wildmenu`, #613): `Some` while `<Tab>` cycles through several matches.
    pub(crate) wild: Option<Wild>,
    /// The matches `c_CTRL-D` listed, shown above the line until it closes or completes again.
    pub(crate) listing: Option<Vec<super::complete::Item>>,
}

/// An active wildmenu: the line as it was before completion, the completed word's span in it, and the
/// matches with the one selected (`None` = the original text, reached by cycling past either end).
pub(crate) struct Wild {
    orig: String,
    orig_cursor: usize,
    /// Byte span of the completed word in `orig` (it ends at the caret).
    start: usize,
    end: usize,
    pub(crate) items: Vec<super::complete::Item>,
    pub(crate) selected: Option<usize>,
    /// Byte offset into the word where the popup menu's text column starts.
    pub(crate) pum_at: usize,
    /// The matches are file names (`<Up>`/`<Down>` walk directories).
    pub(crate) files: bool,
}

impl Wild {
    /// The char column of the popup menu's text in the line (the completed word's start plus the
    /// directory typed before a file's tail).
    pub(crate) fn pum_char_col(&self) -> usize {
        self.orig[..self.start + self.pum_at].chars().count()
    }
}

/// What a completed expression-register prompt does with the evaluated result (`:help quote=`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum ExprTarget {
    /// Normal-mode `"=<expr><CR>`: arm the `"=` register so the FOLLOWING `p`/`P` pastes the result
    /// ([`ruse_core::Command::SetExprRegister`]).
    Paste,
    /// Insert-mode `<C-r>=<expr><CR>`: splice the result at the caret right now
    /// ([`ruse_core::Command::InsertEval`]).
    Insert,
}

/// The `c_CTRL-V` collector's position, the same grammar as `i_CTRL-V` (`:help i_CTRL-V_digit`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Literal {
    /// `CTRL-V` seen: a decimal digit or `o`/`O`/`x`/`X`/`u`/`U` starts a numeric code; any other key is
    /// inserted as itself.
    AwaitFirst,
    /// Collecting a numeric code in `radix`; `remaining` digits are still allowed, `count` were typed.
    Collecting {
        radix: u32,
        value: u32,
        remaining: u8,
        count: u8,
    },
}

/// The result of feeding one key to the `c_CTRL-V` collector.
#[derive(PartialEq, Eq, Debug)]
pub(crate) enum LiteralStep {
    /// The key was a code digit or form selector: keep collecting.
    Pending,
    /// The entry finished; this key was consumed by it.
    Done,
    /// A code finished on a key that is NOT part of it: process that key normally next (nvim
    /// `:a<C-v>x4<Left>b` gives `ab^D`).
    Reprocess(KeyEvent),
}

impl CmdLine {
    /// A fresh, empty line with `prefix` (every other field at its default).
    pub(crate) fn new(prefix: char) -> CmdLine {
        CmdLine {
            prefix,
            buffer: String::new(),
            cursor: 0,
            ex_mode: false,
            mx: false,
            expr: None,
            ctrl_r: false,
            walk: super::history::HistWalk::default(),
            overstrike: false,
            literal: None,
            digraph: None,
            wild: None,
            listing: None,
        }
    }

    /// Replace the text from byte `start` to the caret with `text`, the caret after it (a completion).
    pub(crate) fn replace_word(&mut self, start: usize, text: &str) {
        let end = self.byte(self.cursor);
        self.buffer.replace_range(start..end, text);
        self.cursor = self.buffer[..start + text.len()].chars().count();
        self.edited();
    }

    /// Open the wildmenu over `items` for the word at byte `start`, selecting `first`.
    pub(crate) fn start_wild(
        &mut self,
        start: usize,
        items: Vec<super::complete::Item>,
        pum_at: usize,
        files: bool,
        first: usize,
    ) {
        self.wild = Some(Wild {
            orig: self.buffer.clone(),
            orig_cursor: self.cursor,
            start,
            end: self.byte(self.cursor),
            items,
            selected: None,
            pum_at,
            files,
        });
        self.wild_select(Some(first));
    }

    /// Show match `sel` (or the original word for `None`) in the line.
    fn wild_select(&mut self, sel: Option<usize>) {
        let Some(w) = self.wild.as_mut() else {
            return;
        };
        w.selected = sel;
        let word = sel.map_or(&w.orig[w.start..w.end], |i| w.items[i].text.as_str());
        let mut line = String::with_capacity(w.orig.len() + word.len());
        line.push_str(&w.orig[..w.start]);
        line.push_str(word);
        let cursor = line.chars().count();
        line.push_str(&w.orig[w.end..]);
        let restore = sel.is_none().then_some(w.orig_cursor);
        self.buffer = line;
        self.cursor = restore.unwrap_or(cursor);
    }

    /// `<Tab>`/`CTRL-N` (`forward`) or `<S-Tab>`/`CTRL-P`: the next or previous match, passing through
    /// the original text between the last match and the first (nvim `wildmode=full`).
    pub(crate) fn wild_step(&mut self, forward: bool) {
        let Some(w) = self.wild.as_ref() else {
            return;
        };
        let n = w.items.len();
        let sel = match (w.selected, forward) {
            (None, true) => Some(0),
            (None, false) => Some(n - 1),
            (Some(i), true) => (i + 1 < n).then_some(i + 1),
            (Some(i), false) => i.checked_sub(1),
        };
        self.wild_select(sel);
    }

    /// `c_CTRL-E` in the wildmenu: back to the text before completion.
    pub(crate) fn wild_cancel(&mut self) {
        self.wild_select(None);
        self.wild = None;
    }

    /// The completed word in the line: from where completion started to the caret.
    pub(crate) fn wild_word(&self) -> Option<(usize, &str)> {
        let w = self.wild.as_ref()?;
        let end = self.byte(self.cursor);
        Some((w.start, self.buffer.get(w.start..end)?))
    }

    fn len(&self) -> usize {
        self.buffer.chars().count()
    }

    /// The byte offset of char index `i` (the buffer length past the end).
    fn byte(&self, i: usize) -> usize {
        self.buffer
            .char_indices()
            .nth(i)
            .map_or(self.buffer.len(), |(b, _)| b)
    }

    /// The text before the caret — the prefix `<Up>`/`<Down>` filter history by (nvim `lookfor` is the
    /// line up to `cmdpos`).
    pub(crate) fn before_cursor(&self) -> &str {
        &self.buffer[..self.byte(self.cursor)]
    }

    /// Replace the whole line and put the caret at its end (history recall, a fresh seed).
    pub(crate) fn set_text(&mut self, text: String) {
        self.buffer = text;
        self.cursor = self.len();
    }

    /// An edit ends the history walk: the next `<Up>` re-captures the edited line as its prefix.
    fn edited(&mut self) {
        self.walk = super::history::HistWalk::default();
    }

    /// Type `text` at the caret — inserting, or under `c_<Insert>` overstrike replacing the chars under
    /// it (extending the line past its end). The caret ends after the text.
    pub(crate) fn insert(&mut self, text: &str) {
        for c in text.chars() {
            let at = self.byte(self.cursor);
            if self.overstrike && self.cursor < self.len() {
                let next = self.byte(self.cursor + 1);
                self.buffer
                    .replace_range(at..next, c.encode_utf8(&mut [0; 4]));
            } else {
                self.buffer.insert(at, c);
            }
            self.cursor += 1;
        }
        self.edited();
    }

    /// Move the caret one char left (`<Left>`); no-op at the start.
    pub(crate) fn left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    /// Move the caret one char right (`<Right>`); no-op at the end.
    pub(crate) fn right(&mut self) {
        self.cursor = (self.cursor + 1).min(self.len());
    }

    /// The caret to the start (`<Home>`, `c_CTRL-B`).
    pub(crate) fn home(&mut self) {
        self.cursor = 0;
    }

    /// The caret to the end (`<End>`, `c_CTRL-E`).
    pub(crate) fn end(&mut self) {
        self.cursor = self.len();
    }

    /// `<S-Left>`/`<C-Left>`: one char left, then on to just after the previous SPACE (`ex_getln.c` checks
    /// only `' '`, not tabs): `:ab cd ef<S-Left>` puts the caret before `ef`.
    pub(crate) fn word_left(&mut self) {
        let chars: Vec<char> = self.buffer.chars().collect();
        if self.cursor == 0 {
            return;
        }
        self.cursor -= 1;
        while self.cursor > 0 && chars[self.cursor - 1] != ' ' {
            self.cursor -= 1;
        }
    }

    /// `<S-Right>`/`<C-Right>`: one char right, then on to the next SPACE (the caret lands ON it):
    /// `:ab cd<Home><S-Right>` puts the caret after `ab`.
    pub(crate) fn word_right(&mut self) {
        let chars: Vec<char> = self.buffer.chars().collect();
        if self.cursor >= chars.len() {
            return;
        }
        self.cursor += 1;
        while self.cursor < chars.len() && chars[self.cursor] != ' ' {
            self.cursor += 1;
        }
    }

    /// `<BS>` / `c_CTRL-H`: delete the char before the caret. Returns `true` when the line is EMPTY, which
    /// abandons the command line (`:help c_<BS>`, #811); at the start of a non-empty line it does nothing.
    pub(crate) fn backspace(&mut self) -> bool {
        if self.buffer.is_empty() {
            return true;
        }
        if self.cursor > 0 {
            let (s, e) = (self.byte(self.cursor - 1), self.byte(self.cursor));
            self.buffer.replace_range(s..e, "");
            self.cursor -= 1;
            self.edited();
        }
        false
    }

    /// `<Del>`: delete the char under the caret, or at the end of the line the one before it. Returns
    /// `true` on an EMPTY line, which abandons it like `<BS>` (nvim v0.12.6).
    pub(crate) fn delete(&mut self) -> bool {
        if self.buffer.is_empty() {
            return true;
        }
        if self.cursor >= self.len() {
            return self.backspace();
        }
        let (s, e) = (self.byte(self.cursor), self.byte(self.cursor + 1));
        self.buffer.replace_range(s..e, "");
        self.edited();
        false
    }

    /// `c_CTRL-U`: delete everything before the caret; the text after it stays.
    pub(crate) fn kill_before(&mut self) {
        let at = self.byte(self.cursor);
        self.buffer.replace_range(..at, "");
        self.cursor = 0;
        self.edited();
    }

    /// Emacs `C-k` in the `M-x` minibuffer: delete from the caret to the end.
    pub(crate) fn kill_after(&mut self) {
        let at = self.byte(self.cursor);
        self.buffer.truncate(at);
        self.edited();
    }

    /// `c_CTRL-W`: delete the word before the caret — trailing blanks, then one run of keyword or of
    /// non-keyword chars (`ex_getln.c`): `:ab cd<C-w>` leaves `ab `, `:a.b<C-w>` leaves `a.`.
    pub(crate) fn kill_word_before(&mut self) {
        let end = self.byte(self.cursor);
        let head = &self.buffer[..end];
        let word = |c: char| c.is_alphanumeric() || c == '_';
        let kept = head.trim_end_matches([' ', '\t']).len();
        let class = head[..kept].chars().next_back().map(word);
        let start = head[..kept]
            .char_indices()
            .rev()
            .take_while(|&(_, c)| c != ' ' && c != '\t' && Some(word(c)) == class)
            .last()
            .map_or(kept, |(i, _)| i);
        let removed = self.buffer[start..end].chars().count();
        self.buffer.replace_range(start..end, "");
        self.cursor -= removed;
        self.edited();
    }

    /// Feed one key to the armed `c_CTRL-V` collector (the caller checked `literal.is_some()`). The forms
    /// are `i_CTRL-V`'s: up to three decimal digits, `o`/`O` + three octal, `x`/`X` + two hex, `u` + four,
    /// `U` + eight. Any other first key is inserted as itself — a control chord as its control char, `<CR>`
    /// as `^M`, a special key as its `<Name>` (nvim `:<C-v><Left>` types `<Left>`).
    pub(crate) fn feed_literal(&mut self, key: KeyEvent) -> LiteralStep {
        let Some(state) = self.literal.take() else {
            return LiteralStep::Done;
        };
        let plain_char = match key.code {
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => Some(c),
            _ => None,
        };
        match state {
            Literal::AwaitFirst => {
                let form = match plain_char {
                    Some(c @ '0'..='9') => Some((10, c.to_digit(10).unwrap_or(0), 2, 1)),
                    Some('o' | 'O') => Some((8, 0, 3, 0)),
                    Some('x' | 'X') => Some((16, 0, 2, 0)),
                    Some('u') => Some((16, 0, 4, 0)),
                    Some('U') => Some((16, 0, 8, 0)),
                    _ => None,
                };
                if let Some((radix, value, remaining, count)) = form {
                    self.literal = Some(Literal::Collecting {
                        radix,
                        value,
                        remaining,
                        count,
                    });
                    return LiteralStep::Pending;
                }
                self.insert(&literal_text(key));
                LiteralStep::Done
            }
            Literal::Collecting {
                radix,
                value,
                remaining,
                count,
            } => {
                if let Some(d) = plain_char.and_then(|c| c.to_digit(radix)) {
                    let value = value.saturating_mul(radix).saturating_add(d);
                    if remaining <= 1 {
                        self.insert_code(radix, value);
                        return LiteralStep::Done;
                    }
                    self.literal = Some(Literal::Collecting {
                        radix,
                        value,
                        remaining: remaining - 1,
                        count: count + 1,
                    });
                    return LiteralStep::Pending;
                }
                if count == 0 {
                    // A form selector with no digit (`<C-v>xz`): the key itself is inserted (nvim `z`).
                    self.insert(&literal_text(key));
                    return LiteralStep::Done;
                }
                self.insert_code(radix, value);
                LiteralStep::Reprocess(key)
            }
        }
    }

    /// Insert the char a finished numeric code names; decimal and octal codes are one byte (clamped to
    /// 255, as `i_CTRL-V`). An invalid code point inserts nothing.
    fn insert_code(&mut self, radix: u32, value: u32) {
        let value = if radix == 16 { value } else { value.min(255) };
        if let Some(c) = char::from_u32(value) {
            self.insert(c.encode_utf8(&mut [0; 4]));
        }
    }

    /// Feed one key to the armed `c_CTRL-K` digraph (the caller checked `digraph.is_some()`): two plain
    /// chars look up the pair; an unknown pair inserts the second char (Vim). Any other key aborts.
    pub(crate) fn feed_digraph(&mut self, key: KeyEvent) {
        let pending = self.digraph.take();
        let KeyCode::Char(c) = key.code else {
            return;
        };
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            return;
        }
        match pending {
            Some(None) => self.digraph = Some(Some(c)),
            Some(Some(c1)) => {
                let ch = super::digraph::digraph(c1, c).unwrap_or(c);
                self.insert(ch.encode_utf8(&mut [0; 4]));
            }
            None => {}
        }
    }
}

/// Render `text` for the command-line row and the DISPLAY column of char index `caret`: a control char is
/// shown as `^X` (two cells, nvim's `^M` for a literal `<CR>`), other chars at their cell width.
#[must_use]
pub fn display_with_caret(text: &str, caret: usize) -> (String, usize) {
    use unicode_width::UnicodeWidthChar;
    let mut out = String::with_capacity(text.len());
    let mut col = 0;
    let mut caret_col = None;
    for (i, c) in text.chars().enumerate() {
        if i == caret {
            caret_col = Some(col);
        }
        if (c as u32) < 0x20 || c == '\u{7f}' {
            out.push('^');
            out.push(if c == '\u{7f}' {
                '?'
            } else {
                char::from(c as u8 + b'@')
            });
            col += 2;
        } else {
            out.push(c);
            col += c.width().unwrap_or(0);
        }
    }
    (out, caret_col.unwrap_or(col))
}

/// The text `c_CTRL-V {key}` inserts for a non-code first key: a control chord as its control char,
/// `<CR>`/`<Tab>`/`<Esc>` as theirs, a printable char as itself, anything else as its `<Name>`.
fn literal_text(key: KeyEvent) -> String {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    match key.code {
        KeyCode::Char(c) if ctrl && c.is_ascii() => {
            char::from((c.to_ascii_uppercase() as u8) & 0x1f).to_string()
        }
        KeyCode::Char(c) => c.to_string(),
        KeyCode::Enter => "\r".into(),
        KeyCode::Tab => "\t".into(),
        KeyCode::Esc => "\u{1b}".into(),
        KeyCode::Backspace => "<BS>".into(),
        KeyCode::Delete => "<Del>".into(),
        KeyCode::Insert => "<Insert>".into(),
        KeyCode::Left => "<Left>".into(),
        KeyCode::Right => "<Right>".into(),
        KeyCode::Up => "<Up>".into(),
        KeyCode::Down => "<Down>".into(),
        KeyCode::Home => "<Home>".into(),
        KeyCode::End => "<End>".into(),
        KeyCode::PageUp => "<PageUp>".into(),
        KeyCode::PageDown => "<PageDown>".into(),
        KeyCode::BackTab => "<S-Tab>".into(),
        KeyCode::F(n) => format!("<F{n}>"),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(text: &str, cursor: usize) -> CmdLine {
        let mut cl = CmdLine::new(':');
        cl.buffer = text.into();
        cl.cursor = cursor;
        cl
    }

    fn k(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn word_motions_stop_at_spaces_like_nvim() {
        // nvim v0.12.6: `ab cd ef<S-Left>x` = `ab cd xef`; twice = `ab xcd ef`; `ab  cd<C-Left>` before `cd`.
        let mut cl = line("ab cd ef", 8);
        cl.word_left();
        assert_eq!(cl.cursor, 6);
        cl.word_left();
        assert_eq!(cl.cursor, 3);
        let mut cl = line("ab  cd", 6);
        cl.word_left();
        assert_eq!(cl.cursor, 4);
        // `<Home><S-Right>x` = `abx cd ef`; `<C-Right><C-Right>x` = `ab cdx ef`.
        let mut cl = line("ab cd ef", 0);
        cl.word_right();
        assert_eq!(cl.cursor, 2);
        cl.word_right();
        assert_eq!(cl.cursor, 5);
    }

    #[test]
    fn deletes_respect_the_caret() {
        let mut cl = line("ab cd", 3);
        cl.kill_before();
        assert_eq!((cl.buffer.as_str(), cl.cursor), ("cd", 0));
        let mut cl = line("ab cd", 4);
        cl.kill_word_before();
        assert_eq!((cl.buffer.as_str(), cl.cursor), ("ab d", 3));
        let mut cl = line("abc", 0);
        assert!(!cl.backspace());
        assert_eq!(cl.buffer, "abc");
        assert!(!cl.delete());
        assert_eq!((cl.buffer.as_str(), cl.cursor), ("bc", 0));
        let mut cl = line("abc", 3);
        assert!(!cl.delete());
        assert_eq!((cl.buffer.as_str(), cl.cursor), ("ab", 2));
        assert!(line("", 0).backspace());
        assert!(line("", 0).delete());
    }

    #[test]
    fn overstrike_replaces_then_extends() {
        // nvim: `abc<Left><Left><Insert>XYZW` = `aXYZW`.
        let mut cl = line("abc", 1);
        cl.overstrike = true;
        cl.insert("XYZW");
        assert_eq!((cl.buffer.as_str(), cl.cursor), ("aXYZW", 5));
    }

    #[test]
    fn literal_forms_match_nvim() {
        let run = |keys: &[KeyEvent]| {
            let mut cl = line("a", 1);
            let mut last = LiteralStep::Done;
            for &key in keys {
                if cl.literal.is_none() {
                    cl.literal = Some(Literal::AwaitFirst);
                }
                last = cl.feed_literal(key);
            }
            (cl.buffer, last)
        };
        let ch = |c| k(KeyCode::Char(c));
        assert_eq!(run(&[k(KeyCode::Enter)]).0, "a\r");
        assert_eq!(
            run(&[KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL)]).0,
            "a\u{1}"
        );
        assert_eq!(run(&[ch('0'), ch('6'), ch('5')]).0, "aA");
        assert_eq!(run(&[ch('u'), ch('0'), ch('0'), ch('e'), ch('9')]).0, "aé");
        assert_eq!(run(&[ch('x'), ch('z')]).0, "az");
        assert_eq!(run(&[k(KeyCode::Left)]).0, "a<Left>");
        let (text, step) = run(&[ch('x'), ch('4'), k(KeyCode::Left)]);
        assert_eq!(text, "a\u{4}");
        assert_eq!(step, LiteralStep::Reprocess(k(KeyCode::Left)));
    }

    #[test]
    fn display_shows_control_chars_as_caret_pairs() {
        assert_eq!(display_with_caret("a\rb", 2), ("a^Mb".into(), 3));
        assert_eq!(display_with_caret("ab", 2), ("ab".into(), 2));
    }
}
