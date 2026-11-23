//! Bracketed paste (`:help paste`, #600): the paste-text sanitiser ([`paste_text`],
//! [`opens_control_sequence`]) and [`InputEngine::paste`], which routes the text per mode as nvim's
//! `vim.paste` does. Moved out of `input/mod.rs` as an `impl InputEngine` block (#1493).

use ruse_core::{Command, Mode, SelectKind};

use super::{ChangeIntent, Feed, InputEngine, InputProfile};

/// Whether `c` can open a terminal control sequence: ESC or a C1 control (U+0080..=U+009F, CSI and OSC
/// among them). A paste never carries one (TERM-PASTE-2, SEC-5).
#[must_use]
pub fn opens_control_sequence(c: char) -> bool {
    c == '\u{1b}' || ('\u{80}'..='\u{9f}').contains(&c)
}

/// A bracketed paste's text as ruse inserts it. Line endings are normalised the way nvim's TUI pastes
/// (`nvim_paste(data, crlf = true)`): CRLF and a lone CR both become LF. ESC and the C1 controls are
/// dropped (TERM-PASTE-2, SEC-5: a paste is untrusted and never carries a control sequence); nvim inserts
/// them literally. One pass, linear in the length.
#[must_use]
pub fn paste_text(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw
        .chars()
        .filter(|&c| !opens_control_sequence(c))
        .peekable();
    while let Some(c) = chars.next() {
        if c == '\r' {
            if chars.peek() == Some(&'\n') {
                chars.next();
            }
            out.push('\n');
        } else {
            out.push(c);
        }
    }
    out
}

impl InputEngine {
    /// A bracketed paste (`:help paste`, #600) in core mode `mode`. The text is never read as keys: no
    /// mapping, count, operator or abbreviation applies, and a pending prefix (`CTRL-R`, `CTRL-V`, a count,
    /// an operator, an Emacs `C-x` or `ESC`) is dropped. `text` has `\n` line endings ([`paste_text`]).
    /// Per nvim v0.12.6's `vim.paste`:
    /// - command line (and the Emacs minibuffer): the first line is inserted literally;
    /// - Insert: the text is inserted at the caret, part of the insert session (one undo step, `.`);
    /// - Replace: the text overwrites ([`Command::ReplacePaste`]);
    /// - Normal: `a{text}<Esc>`, recorded as that change for `.`;
    /// - Visual/Select: the selection is replaced by the text (Select's replace-selection path, #1073)
    ///   and Normal resumes. A block is deleted and the text goes at its top-left corner; the engine has
    ///   no buffer, so `visual_append` (the caller's answer to "does deleting the block leave the caret
    ///   past the end of its line") picks `a` over `i`. `.` repeats it as the Normal paste.
    ///
    /// The command-line window ignores a paste.
    pub fn paste(&mut self, text: &str, mode: Mode, visual_append: bool) -> Feed {
        if self.cmdline.is_some() {
            let line = text.split('\n').next().unwrap_or("");
            self.cmdline_splice(line);
            return Feed::Pending;
        }
        if self.input_profile == InputProfile::Emacs {
            self.emacs_meta = false;
            self.emacs_prefix = None;
            self.emacs_arg = None;
            return Feed::Cmd(Command::EmacsPaste(text.to_string()));
        }
        if self.cmdwin.is_some() {
            return Feed::Ignored;
        }
        let paste = Command::InsertPaste(text.to_string());
        match mode {
            Mode::Insert | Mode::Replace | Mode::VirtualReplace => {
                self.insert.ctrl_g = false;
                self.insert.ctrl_r = false;
                self.insert.ctrl_x = false;
                self.insert.digraph = None;
                self.insert.literal = None;
                self.insert.completion = None;
                // nvim drops a paste in Virtual Replace (`vim.paste` has no `Rv` branch); overwriting like
                // `R` keeps the text instead of losing it.
                let out = Feed::Cmd(if mode == Mode::Insert {
                    paste
                } else {
                    Command::ReplacePaste(text.to_string())
                });
                self.record(&out, mode);
                out
            }
            Mode::Normal | Mode::Visual { .. } | Mode::Select { .. } => {
                self.reset();
                self.leader = false;
                self.pending_record_register = None;
                self.pending_record_expr = None;
                let change = ChangeIntent {
                    lead: Command::EnterInsertAfter,
                    insert: vec![paste.clone(), Command::EnterNormal],
                    register: None,
                    expr: None,
                    entry_count: 1,
                    screen: None,
                };
                self.last_inserted = change.insert_body().to_vec();
                self.last_change = Some(change);
                let block = matches!(
                    mode,
                    Mode::Visual {
                        kind: SelectKind::Blockwise
                    } | Mode::Select {
                        kind: SelectKind::Blockwise
                    }
                );
                let cmds = match mode {
                    Mode::Normal => vec![Command::EnterInsertAfter, paste, Command::EnterNormal],
                    // A block is deleted, then the text goes at its top-left corner (not down every row).
                    _ if block => vec![
                        Command::DeleteSelection,
                        if visual_append {
                            Command::EnterInsertAfter
                        } else {
                            Command::EnterInsert
                        },
                        paste,
                        Command::EnterNormal,
                    ],
                    // Charwise/linewise: Select's replace-selection path (the core's `InsertPaste`
                    // over a selection), then Normal.
                    _ => vec![paste, Command::EnterNormal],
                };
                Feed::Replay(cmds)
            }
            // A terminal buffer's paste goes to its child; the session routes it before the engine.
            Mode::Terminal | Mode::TerminalNormal => Feed::Ignored,
        }
    }
}
