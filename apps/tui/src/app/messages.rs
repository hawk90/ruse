//! The status line's message and the `:messages` history (#1235).
//!
//! Every message is given with its [`Kind`], decided where it is given (`status.error(..)`,
//! `status.info(..)`, …), never guessed from its text. The kind decides two things: an [`Kind::Error`]
//! fails the command that gave it (the rest of a `|` line, a macro, a `:normal` is dropped), and every kind
//! but [`Kind::Echo`] is kept in the history `:messages` shows.
//!
//! What nvim v0.12.6 keeps (probed with `--clean` in a PTY, `execute('messages')` after each command):
//! errors (`E492`, `E486`, …, also under `:silent`, not under `:silent!`), warnings (`W10`), and reports:
//! `3 lines yanked`, `3 fewer lines`, `3 more lines`, `3 lines moved`, `3 lines >ed 1 time`, `2 matches on 2
//! lines`, the undo message, `Already at oldest change`, the `:=` number, `:pwd`, `CTRL-G`'s file info.
//! What it does not keep: `:echo`, `:set ts?`, the file messages of `:w` and `:e`, `ga`, the `search hit
//! BOTTOM` note, `--No lines in buffer--`, the output of `:ls`, `:p`, `:g`, `:reg`, `:!` and `:messages`,
//! and anything under `:silent`.

use std::collections::VecDeque;

/// What a message is: how it fails a command and whether `:messages` keeps it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    /// An error (nvim's `emsg`): kept, and it fails the command.
    Error,
    /// A warning (`W10`, `W11`): kept.
    Warning,
    /// A report a command gives (`3 lines yanked`, `:pwd`): kept.
    Info,
    /// Output or a reply that is shown and not kept: a listing, an option's value, a file message, a
    /// prompt, progress.
    Echo,
}

/// A message and its kind, for a function that hands one back for its caller to give.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Msg {
    pub(crate) kind: Kind,
    pub(crate) text: String,
}

impl Msg {
    pub(crate) fn error(text: impl Into<String>) -> Msg {
        Msg {
            kind: Kind::Error,
            text: text.into(),
        }
    }
    pub(crate) fn info(text: impl Into<String>) -> Msg {
        Msg {
            kind: Kind::Info,
            text: text.into(),
        }
    }
    pub(crate) fn echo(text: impl Into<String>) -> Msg {
        Msg {
            kind: Kind::Echo,
            text: text.into(),
        }
    }
}

/// nvim's default `'messagesopt'` `history:500`.
pub(crate) const HISTORY_DEFAULT: usize = 500;

/// The messages `:messages` shows, oldest first, at most `cap` of them (`'messagesopt'` `history:`).
#[derive(Debug)]
pub(crate) struct History {
    entries: VecDeque<String>,
    cap: usize,
}

impl Default for History {
    fn default() -> History {
        History {
            entries: VecDeque::new(),
            cap: HISTORY_DEFAULT,
        }
    }
}

impl History {
    fn push(&mut self, text: &str) {
        if self.cap == 0 {
            return;
        }
        while self.entries.len() >= self.cap {
            self.entries.pop_front();
        }
        self.entries.push_back(text.to_string());
    }

    /// The last `count` messages (all of them for `None`), oldest first.
    pub(crate) fn last(&self, count: Option<usize>) -> impl Iterator<Item = &str> {
        let skip = count.map_or(0, |n| self.entries.len().saturating_sub(n));
        self.entries.iter().skip(skip).map(String::as_str)
    }

    /// Drop all but the last `keep` messages (`:messages clear` is `keep` 0).
    pub(crate) fn keep_last(&mut self, keep: usize) {
        while self.entries.len() > keep {
            self.entries.pop_front();
        }
    }

    /// Keep at most `cap` messages from now on; 0 clears the history (nvim).
    pub(crate) fn set_cap(&mut self, cap: usize) {
        self.cap = cap;
        self.keep_last(cap);
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }
}

/// Which messages a `:silent` keeps out of the history, as it keeps them off the screen.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Default)]
pub(crate) enum Silence {
    #[default]
    Off,
    /// `:silent`: all but errors.
    Messages,
    /// `:silent!`: errors too.
    All,
}

/// The status line's message, its kind, and the history it feeds.
#[derive(Debug, Default)]
pub(crate) struct Status {
    text: String,
    kind: Option<Kind>,
    /// Counts the messages given, so a caller can tell whether one was given since (a repeated message
    /// is a new one).
    serial: u64,
    silence: Silence,
    history: History,
}

impl Status {
    /// A status line showing `msg` (not kept: nothing has happened yet).
    pub(crate) fn showing(msg: Msg) -> Status {
        Status {
            text: msg.text,
            kind: Some(msg.kind),
            ..Status::default()
        }
    }

    /// Give `text` as a message of `kind`: show it, and keep it in the history when its kind is kept and
    /// no `:silent` hides it. An empty text clears the line.
    pub(crate) fn set(&mut self, kind: Kind, text: impl Into<String>) {
        let text = text.into();
        self.serial += 1;
        if text.is_empty() {
            self.text.clear();
            self.kind = None;
            return;
        }
        let kept = match self.silence {
            Silence::Off => kind != Kind::Echo,
            Silence::Messages => kind == Kind::Error,
            Silence::All => false,
        };
        if kept {
            self.history.push(&text);
        }
        self.text = text;
        self.kind = Some(kind);
    }

    pub(crate) fn error(&mut self, text: impl Into<String>) {
        self.set(Kind::Error, text);
    }
    pub(crate) fn warn(&mut self, text: impl Into<String>) {
        self.set(Kind::Warning, text);
    }
    pub(crate) fn info(&mut self, text: impl Into<String>) {
        self.set(Kind::Info, text);
    }
    pub(crate) fn echo(&mut self, text: impl Into<String>) {
        self.set(Kind::Echo, text);
    }
    pub(crate) fn give(&mut self, msg: Msg) {
        self.set(msg.kind, msg.text);
    }

    /// Clear the line. The history keeps what it had.
    pub(crate) fn clear(&mut self) {
        self.set(Kind::Echo, String::new());
    }

    /// Change the text on the line without giving a new message: a note added to the one shown, kept or
    /// not as that one was. The history keeps the message as it was given.
    pub(crate) fn amend(&mut self, text: impl Into<String>) {
        self.text = text.into();
        if self.text.is_empty() {
            self.kind = None;
        } else if self.kind.is_none() {
            self.kind = Some(Kind::Echo);
        }
    }

    /// Clear the line and hand back what it showed.
    pub(crate) fn take(&mut self) -> Msg {
        let msg = self.shown();
        self.clear();
        msg
    }

    /// What the line shows.
    pub(crate) fn shown(&self) -> Msg {
        Msg {
            kind: self.kind.unwrap_or(Kind::Echo),
            text: self.text.clone(),
        }
    }

    /// Show `msg` again (one [`Status::take`] took off the line) without keeping it a second time.
    pub(crate) fn restore(&mut self, msg: Msg) {
        self.serial += 1;
        self.kind = (!msg.text.is_empty()).then_some(msg.kind);
        self.text = msg.text;
    }

    pub(crate) fn text(&self) -> &str {
        &self.text
    }

    /// The line shows an error.
    pub(crate) fn is_error(&self) -> bool {
        self.kind == Some(Kind::Error)
    }

    pub(crate) fn serial(&self) -> u64 {
        self.serial
    }

    /// Keep messages out of the history as `silence` says (or as an enclosing `:silent` already does, when
    /// that hides more), until [`Status::unhush`] puts back the setting this returns.
    pub(crate) fn hush(&mut self, silence: Silence) -> Silence {
        let old = self.silence;
        self.silence = old.max(silence);
        old
    }

    pub(crate) fn unhush(&mut self, old: Silence) {
        self.silence = old;
    }

    pub(crate) fn history(&self) -> &History {
        &self.history
    }

    pub(crate) fn history_mut(&mut self) -> &mut History {
        &mut self.history
    }
}

impl std::ops::Deref for Status {
    type Target = str;
    fn deref(&self) -> &str {
        &self.text
    }
}

impl std::fmt::Display for Status {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.text)
    }
}

impl PartialEq<str> for Status {
    fn eq(&self, other: &str) -> bool {
        self.text == other
    }
}

impl PartialEq<&str> for Status {
    fn eq(&self, other: &&str) -> bool {
        self.text == *other
    }
}

impl PartialEq<String> for Status {
    fn eq(&self, other: &String) -> bool {
        &self.text == other
    }
}

/// `:[count]mes[sages] [clear]` (`:help :messages`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct MessagesCmd {
    /// The count; nvim reads `0` as 1 (`correct_range`), and of a range the last number counts.
    pub(crate) count: Option<usize>,
    pub(crate) clear: bool,
}

/// Why a `:messages` line is refused. `Display` is nvim's message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum MessagesError {
    /// An argument other than `clear` (`E474`).
    InvalidArgument,
    /// `:messages!` (`E477`); the line as typed.
    NoBang(String),
}

impl std::fmt::Display for MessagesError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MessagesError::InvalidArgument => f.write_str("E474: Invalid argument"),
            MessagesError::NoBang(line) => write!(f, "E477: No ! allowed: {line}"),
        }
    }
}

/// Parse a `:messages` line, before any range is resolved (its count is no line number). `None` when the
/// line is not `:messages`; `Err` when its argument is wrong.
pub(crate) fn parse_messages(line: &str) -> Option<Result<MessagesCmd, MessagesError>> {
    let line = line.trim_start_matches([' ', '\t', ':']);
    let digits = |s: &str| s.bytes().take_while(u8::is_ascii_digit).count();
    let mut count = None;
    let mut rest = line;
    let n = digits(rest);
    if n > 0 {
        let mut last = &rest[..n];
        rest = rest[n..].trim_start();
        if let Some(r) = rest.strip_prefix(',') {
            let r = r.trim_start();
            let m = digits(r);
            if m == 0 {
                return None;
            }
            last = &r[..m];
            rest = r[m..].trim_start();
        }
        count = Some(last.parse::<usize>().unwrap_or(usize::MAX).max(1));
    }
    let word = rest.bytes().take_while(u8::is_ascii_alphabetic).count();
    let name = &rest[..word];
    if name.len() < 3 || !"messages".starts_with(name) {
        return None;
    }
    let rest = &rest[word..];
    if rest.starts_with('!') {
        return Some(Err(MessagesError::NoBang(line.trim_end().to_string())));
    }
    if !rest.is_empty() && !rest.starts_with([' ', '\t', '|', '"']) {
        return None;
    }
    let arg = rest.trim();
    Some(match arg {
        "" => Ok(MessagesCmd {
            count,
            clear: false,
        }),
        "clear" => Ok(MessagesCmd { count, clear: true }),
        _ => Err(MessagesError::InvalidArgument),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::set_cmd::messagesopt_history;

    fn kept(s: &Status) -> Vec<&str> {
        s.history().last(None).collect()
    }

    #[test]
    fn errors_warnings_and_reports_are_kept_echoes_are_not() {
        let mut s = Status::default();
        s.error("E492: Not an editor command: x");
        s.warn("W10: Warning: Changing a readonly file");
        s.info("3 lines yanked");
        s.echo("  tabstop=8");
        s.echo("\"f.txt\" 1L, 2B written");
        assert_eq!(
            kept(&s),
            [
                "E492: Not an editor command: x",
                "W10: Warning: Changing a readonly file",
                "3 lines yanked"
            ]
        );
        assert_eq!(s.text(), "\"f.txt\" 1L, 2B written");
        assert!(!s.is_error());
    }

    #[test]
    fn an_empty_message_clears_the_line_and_is_not_kept() {
        let mut s = Status::default();
        s.error("E1: x");
        assert!(s.is_error());
        s.info(String::new());
        assert_eq!(s.text(), "");
        assert!(!s.is_error());
        assert_eq!(kept(&s), ["E1: x"]);
    }

    #[test]
    fn silent_keeps_errors_and_silent_bang_keeps_nothing() {
        let mut s = Status::default();
        let outer = s.hush(Silence::Messages);
        s.info("3 fewer lines");
        s.error("E492: Not an editor command: a");
        let inner = s.hush(Silence::All);
        s.error("E492: Not an editor command: b");
        s.unhush(inner);
        // A plain command inside a `:silent` is still silent.
        let plain = s.hush(Silence::Off);
        s.info("1 line yanked");
        s.unhush(plain);
        s.unhush(outer);
        s.info("2 lines yanked");
        assert_eq!(
            kept(&s),
            ["E492: Not an editor command: a", "2 lines yanked"]
        );
    }

    #[test]
    fn a_repeated_message_is_a_new_one() {
        let mut s = Status::default();
        s.error("E486: Pattern not found: x");
        let before = s.serial();
        s.error("E486: Pattern not found: x");
        assert_ne!(s.serial(), before);
        assert_eq!(s.history().len(), 2);
    }

    #[test]
    fn amend_changes_the_line_not_the_history() {
        let mut s = Status::default();
        s.info("1 line yanked");
        s.amend("1 line yanked · note");
        assert_eq!(s.text(), "1 line yanked · note");
        assert_eq!(kept(&s), ["1 line yanked"]);
    }

    #[test]
    fn the_history_is_bounded_and_counted() {
        let mut s = Status::default();
        s.history_mut().set_cap(3);
        for i in 1..=6 {
            s.info(format!("m{i}"));
        }
        assert_eq!(kept(&s), ["m4", "m5", "m6"]);
        assert_eq!(s.history().last(Some(2)).collect::<Vec<_>>(), ["m5", "m6"]);
        assert_eq!(s.history().last(Some(99)).count(), 3);
        s.history_mut().keep_last(1);
        assert_eq!(kept(&s), ["m6"]);
        s.history_mut().set_cap(0);
        s.info("z");
        assert_eq!(s.history().len(), 0);
    }

    #[test]
    fn the_default_history_holds_500() {
        let mut s = Status::default();
        for i in 0..600 {
            s.info(format!("m{i}"));
        }
        assert_eq!(s.history().len(), HISTORY_DEFAULT);
        assert_eq!(s.history().last(Some(1)).next(), Some("m599"));
    }

    #[test]
    fn messages_command_forms() {
        let ok = |count, clear| Some(Ok(MessagesCmd { count, clear }));
        assert_eq!(parse_messages("messages"), ok(None, false));
        assert_eq!(parse_messages("mes"), ok(None, false));
        assert_eq!(parse_messages("3mes"), ok(Some(3), false));
        assert_eq!(parse_messages("0messages"), ok(Some(1), false));
        assert_eq!(parse_messages("1,2messages"), ok(Some(2), false));
        assert_eq!(parse_messages("messages clear"), ok(None, true));
        assert_eq!(parse_messages("2mes clear"), ok(Some(2), true));
        assert_eq!(
            parse_messages("messages foo"),
            Some(Err(MessagesError::InvalidArgument))
        );
        assert_eq!(
            parse_messages("messages cl"),
            Some(Err(MessagesError::InvalidArgument))
        );
        assert_eq!(
            parse_messages("messages!"),
            Some(Err(MessagesError::NoBang("messages!".into())))
        );
        assert_eq!(parse_messages("me"), None);
        assert_eq!(parse_messages("mesx"), None);
        assert_eq!(parse_messages("move"), None);
    }

    #[test]
    fn messagesopt_values_follow_nvim() {
        assert_eq!(
            messagesopt_history("hit-enter,history:500,progress:c"),
            Some(500)
        );
        assert_eq!(messagesopt_history("wait:5,history:1"), Some(1));
        assert_eq!(messagesopt_history("hit-enter,,history:01"), Some(1));
        assert_eq!(messagesopt_history("hit-enter,history:0"), Some(0));
        for bad in [
            "hit-enter",
            "history:3",
            "hit-enter,history:10001",
            "hit-enter,history:-1",
            "hit-enter,history:x",
            "hit-enter,history:",
            "history:2,hit-enter,foo",
            " hit-enter,history:1",
            "progress:x,hit-enter,history:1",
        ] {
            assert_eq!(messagesopt_history(bad), None, "{bad}");
        }
    }
}
