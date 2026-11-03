//! A terminal-free session that drives the real key path (#895).
//!
//! [`Headless`] owns the same `Session` the `ruse` binary runs and feeds it keys through
//! `Session::on_key`: the macro layer, the session prefixes and chords, pickers, `H`/`M`/`L`, the input
//! engine and the Ex dispatch. Before every key it runs the frame work the event loop runs
//! (`pre_frame`, then `post_render`), so the one-frame values a handler reads (snapshot, line index, pane
//! geometry, folds) are as fresh as in the binary, and each macro-replayed key gets its own frame.
//! Nothing is rendered: the render reads state but does not change it.
//!
//! Differences from the binary, all deliberate:
//! - The buffer has no file. `:w` declines (`E32`); read the result with [`Headless::text`].
//! - The crash-rescue store and the recovery journal are not touched (they are process-global).
//! - The system clipboard is not installed; `"+`/`"*` stay a no-op.
//! - Language servers never start: the workspace is untrusted.
//!
//! Keys are written in Vim notation ([`parse_keys`]) and decoded the way crossterm decodes a legacy
//! terminal's bytes, so a test sees the key events a real terminal delivers (`<C-\>` arrives as
//! `CTRL-4`, an uppercase letter carries `SHIFT`, `<M-x>` is `ESC x` read as `ALT-x`).

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ruse_core::{Mode, RegKind, Workspace};

use crate::app::session::{Flow, FrameCtx, Session, SessionInit, TermInfo};
use crate::input::InputEngine;

/// The key profile a headless session runs (`RUSE_PROFILE`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Profile {
    Vim,
    Emacs,
    Native,
}

/// One editor session driven without a terminal. See the module docs.
pub struct Headless {
    s: Session,
    frame: FrameCtx,
    /// The workspace was changed outside a key (`workspace_mut`), so `frame` must be rebuilt first.
    stale: bool,
    cols: u16,
    rows: u16,
    /// The E-number of the last error message a key put on the status line (`"E486"`), or empty.
    emsg: String,
}

impl Headless {
    /// A session over `text` in `profile`, on an 80x24 terminal, with ruse's shipped defaults.
    #[must_use]
    pub fn new(profile: Profile, text: &str) -> Headless {
        Headless::with_size(profile, text, 80, 24)
    }

    /// A Vim-profile session over `text`.
    #[must_use]
    pub fn vim(text: &str) -> Headless {
        Headless::new(Profile::Vim, text)
    }

    /// An Emacs-profile session over `text`.
    #[must_use]
    pub fn emacs(text: &str) -> Headless {
        Headless::new(Profile::Emacs, text)
    }

    /// A session on a `cols`x`rows` terminal (the status line takes the last row).
    #[must_use]
    pub fn with_size(profile: Profile, text: &str, cols: u16, rows: u16) -> Headless {
        let profile = match profile {
            Profile::Vim => "vim",
            Profile::Emacs => "emacs",
            Profile::Native => "native",
        };
        let mut s = Session::new(SessionInit {
            path: None,
            fmt: crate::persist::encoding::FileFormat::default(),
            initial: text.as_bytes().to_vec(),
            status: String::new(),
            profile,
            term: TermInfo::default(),
            cwd: std::env::current_dir().unwrap_or_default(),
            trusted: false,
            rescue: false,
        });
        let frame = Headless::settle(&mut s, cols, rows);
        Headless {
            s,
            frame,
            stale: false,
            cols,
            rows,
            emsg: String::new(),
        }
    }

    /// Use Vim's factory search case (`noignorecase`, `nosmartcase`) instead of ruse's shipped smartcase,
    /// for comparing against an oracle run with `-u NONE`. Applies to every buffer.
    #[must_use]
    pub fn vanilla_search_case(mut self) -> Headless {
        self.s.ws.set_default_search_case(false, false);
        // Set before any key, so it is part of where a saved trace starts.
        if self.s.recorded.is_empty() {
            self.s.start_options = self.s.ws.focused().view.editing_options();
            self.s.start_registers = self.s.ws.registers().filled();
        }
        self
    }

    /// Install `clipboard` as the system clipboard behind `"+`/`"*` (the binary installs the OS one).
    #[must_use]
    pub fn with_clipboard(mut self, clipboard: ruse_core::MemClipboard) -> Headless {
        self.s.ws.set_clipboard(Box::new(clipboard));
        self
    }

    /// Start the trace over from here, as a session that began with this document and these registers
    /// would: the document as it is now becomes the trace's starting one, the cursor goes to its start, and
    /// the options and the registers that hold a value are what a saved trace starts by writing (RFC-0020
    /// R4, R6). For checking the start of a trace that begins with registers already filled.
    pub fn restart_trace(&mut self) {
        self.stale = true;
        self.s.ws.place_focused_cursor(ruse_core::BytePos(0));
        self.s.initial = self.s.ws.focused().doc.bytes().to_vec();
        self.s.recorded.clear();
        self.s.start_options = self.s.ws.focused().view.editing_options();
        self.s.start_registers = self.s.ws.registers().filled();
    }

    /// Apply `edits` (`(start, end, text)` byte edits) to the focused buffer the way a language server's
    /// formatting, rename, code action or completion edits are applied, through the same session path that
    /// records them in the trace (RFC-0020 R6). No server runs headless, so this stands in for its response.
    /// Returns whether the batch applied.
    pub fn lsp_edits(&mut self, edits: &[(usize, usize, &str)]) -> bool {
        self.stale = true;
        let edits = edits
            .iter()
            .map(|&(s, e, t)| (ruse_core::BytePos(s), ruse_core::BytePos(e), t.to_string()))
            .collect();
        let recorded = &mut self.s.recorded;
        crate::app::lsp_coordinator::apply_lsp_edits(
            &mut self.s.ws,
            &mut |ws, ex| crate::app::dispatch::run_ex(ex, ws, recorded),
            edits,
        )
    }

    /// Run the frame work the event loop runs between keys (the render itself is skipped).
    fn settle(s: &mut Session, cols: u16, rows: u16) -> FrameCtx {
        let frame = s.pre_frame(cols, rows);
        s.post_render(&frame);
        frame
    }

    /// Feed one key event, then drain the keys it queued (a split Alt chord's tail, a macro replay), a
    /// frame per key, as the binary does.
    /// Keys after the session quit are ignored. Returns whether the session is still running.
    pub fn key(&mut self, key: KeyEvent) -> bool {
        if self.s.quit {
            return false;
        }
        if self.stale {
            self.frame = Headless::settle(&mut self.s, self.cols, self.rows);
            self.stale = false;
        }
        let mut flow = self.s.on_key(key, false, &self.frame);
        self.frame = Headless::settle(&mut self.s, self.cols, self.rows);
        self.note_emsg();
        while flow == Flow::Continue {
            let Some((k, from_replay)) = self.s.next_queued() else {
                break;
            };
            flow = self.s.on_key(k, from_replay, &self.frame);
            self.frame = Headless::settle(&mut self.s, self.cols, self.rows);
            self.note_emsg();
        }
        flow == Flow::Continue
    }

    /// Deliver a bracketed paste of `text`, as the binary does for a terminal's `CSI 200 ~ … CSI 201 ~`.
    /// Returns whether the session is still running.
    pub fn paste(&mut self, text: &str) -> bool {
        if self.s.quit {
            return false;
        }
        if self.stale {
            self.frame = Headless::settle(&mut self.s, self.cols, self.rows);
            self.stale = false;
        }
        let flow = self.s.on_paste(text, &self.frame);
        self.frame = Headless::settle(&mut self.s, self.cols, self.rows);
        flow == Flow::Continue
    }

    /// Feed keys written in Vim notation (see [`parse_keys`]).
    pub fn keys(&mut self, notation: &str) -> &mut Headless {
        for k in parse_keys(notation) {
            self.key(k);
        }
        self
    }

    /// The focused buffer's text (lossy UTF-8).
    #[must_use]
    pub fn text(&self) -> String {
        String::from_utf8_lossy(self.s.ws.focused().doc.bytes()).into_owned()
    }

    /// The focused view's cursor, a byte offset.
    #[must_use]
    pub fn cursor(&self) -> usize {
        self.s.ws.focused().view.cursor().0
    }

    /// The focused view's cursor as Neovim reports it: 1-based row, 0-based byte column.
    #[must_use]
    pub fn cursor_row_col(&self) -> (usize, usize) {
        let bytes = self.s.ws.focused().doc.bytes();
        let off = self.cursor().min(bytes.len());
        let line_start = bytes[..off]
            .iter()
            .rposition(|&b| b == b'\n')
            .map_or(0, |i| i + 1);
        let row = bytes[..off].iter().filter(|&&b| b == b'\n').count() + 1;
        (row, off - line_start)
    }

    /// The focused buffer's folds per line, as nvim's `foldclosed()`, `foldclosedend()` and `foldlevel()`
    /// report them: `[first, last, level]` with 1-based lines, `-1` for a line in no closed fold. The
    /// parity oracle records the same for a fixture that observes folds (#903).
    #[must_use]
    pub fn fold_lines(&self) -> Vec<[i64; 3]> {
        let bytes = self.s.ws.focused().doc.bytes();
        let lines = crate::app::dispatch::line_count(bytes);
        let folds = self.folds();
        (0..lines)
            .map(|l| {
                let level = folds.iter().filter(|f| f.contains(l)).count() as i64;
                match crate::folds::closed_range(folds, l) {
                    Some((s, e)) => [s as i64 + 1, e as i64 + 1, level],
                    None => [-1, -1, level],
                }
            })
            .collect()
    }

    /// The focused buffer's folds.
    #[must_use]
    pub fn folds(&self) -> &[crate::folds::Fold] {
        self.s
            .folds
            .get(&self.s.ws.focused_buffer())
            .map_or(&[][..], Vec::as_slice)
    }

    /// The whole-state invariant walker (#906): the workspace's, and the session's folds — every fold set
    /// well formed over its buffer, and every fold anchor live in its document.
    pub fn assert_invariants(&self) {
        self.s.ws.assert_invariants();
        self.s.assert_fold_invariants();
    }

    /// The editor-global register store.
    #[must_use]
    pub fn registers(&self) -> &ruse_core::RegisterStore {
        self.s.ws.registers()
    }

    /// The focused view's Emacs mark, a byte offset.
    #[must_use]
    pub fn mark(&self) -> Option<usize> {
        self.s.ws.focused().view.mark().map(|p| p.0)
    }

    /// The focused view's mode.
    #[must_use]
    pub fn mode(&self) -> Mode {
        self.s.ws.focused().view.mode()
    }

    /// The status line's message.
    #[must_use]
    pub fn status(&self) -> &str {
        &self.s.status
    }

    /// The E-number of the last error message any key so far raised (`"E486"`), or `""` — Vim's
    /// `v:errmsg` reduced to its number, which is what the parity oracle records (#901).
    #[must_use]
    pub fn emsg(&self) -> &str {
        &self.emsg
    }

    /// Remember the status line's E-number, if it now shows an error message.
    fn note_emsg(&mut self) {
        // An error `:silent!` kept off the status line still counts (Vim's `v:errmsg`).
        if let Some(hidden) = self.s.silenced_error.take() {
            if let Some(code) = emsg_code(&hidden) {
                self.emsg = code.to_string();
            }
        }
        if let Some(code) = emsg_code(&self.s.status) {
            if code != self.emsg {
                self.emsg = code.to_string();
            }
        }
    }

    /// Whether the session has quit (`:q`, `C-w q` on the last window, …).
    #[must_use]
    pub fn quit(&self) -> bool {
        self.s.quit
    }

    /// The register `name` (`'"'` is the unnamed one) as `(text, kind)`; `kind` is `"charwise"`,
    /// `"linewise"` or `"blockwise"`, and an empty register is `("", "")`.
    #[must_use]
    pub fn register(&mut self, name: char) -> (String, &'static str) {
        self.s
            .ws
            .register_snapshot()
            .into_iter()
            .find(|(n, _, _)| *n == name)
            .map_or((String::new(), ""), |(_, kind, bytes)| {
                let kind = match kind {
                    RegKind::Charwise => "charwise",
                    RegKind::Linewise => "linewise",
                    RegKind::Blockwise => "blockwise",
                };
                (String::from_utf8_lossy(&bytes).into_owned(), kind)
            })
    }

    /// Place the cursor at byte offset `pos` (clamped, snapped, sticky column seeded) — for homing a
    /// fixture's start point. The next key sees a fresh frame.
    pub fn set_cursor(&mut self, pos: usize) {
        self.s.ws.set_focused_cursor(ruse_core::BytePos(pos));
        self.stale = true;
    }

    /// The workspace, for observations the accessors above do not cover.
    #[must_use]
    pub fn workspace(&self) -> &Workspace {
        &self.s.ws
    }

    /// The workspace, mutably — for checks that act on the core directly after the keys (a full undo).
    /// The next key sees a fresh frame.
    pub fn workspace_mut(&mut self) -> &mut Workspace {
        self.stale = true;
        &mut self.s.ws
    }

    /// The trace `:trace save` would write now: the starting options, then every recorded command
    /// (RFC-0020).
    #[must_use]
    pub fn trace(&self) -> ruse_core::Trace {
        ruse_core::Trace::record_with_options(
            &self.s.initial,
            &self.s.start_options,
            &self.s.start_registers,
            self.s.recorded.clone(),
        )
    }

    /// The input engine, for observations the accessors above do not cover.
    #[must_use]
    pub fn engine(&self) -> &InputEngine {
        &self.s.engine
    }
}

/// The `E123` that opens an error message, as Vim numbers its errors.
fn emsg_code(status: &str) -> Option<&str> {
    let digits = status.strip_prefix('E')?;
    let n = digits.bytes().take_while(u8::is_ascii_digit).count();
    (n > 0 && digits[n..].starts_with(':')).then(|| &status[..=n])
}

/// Parse Vim key notation into the key events a legacy terminal delivers through crossterm.
///
/// Literal characters stand for themselves (an uppercase letter carries `SHIFT`, as crossterm reports
/// it). Bracketed names, case-insensitive: `<Esc>`, `<CR>`/`<Enter>`/`<Return>`, `<BS>`, `<Tab>`,
/// `<Space>`, `<lt>`, `<Del>`, `<Up>`/`<Down>`/`<Left>`/`<Right>`, `<Home>`/`<End>`, `<PageUp>`/`<PageDown>`, `<Insert>`, `<C-x>` (control),
/// `<M-x>`/`<A-x>` (meta, i.e. `ESC x`), `<S-Right>` (shift on a special key, as crossterm reports a
/// shifted arrow/`<Home>`/`<End>`), and combinations such as `<M-C-x>` or `<M-BS>`. Control keys
/// fold as the terminal folds them: `<C-Space>`/`<C-@>` is `CTRL-Space`, `<C-\>`/`<C-]>`/`<C-^>`/`<C-_>`
/// arrive as `CTRL-4`…`CTRL-7`, `<C-[>` is `Esc`, `<C-i>` is `Tab` and `<C-m>` is `Enter`. A `<` that does
/// not open a known name is a literal `<`.
///
/// # Panics
/// On a `<C-…>` or `<M-…>` name whose key has no legacy encoding.
#[must_use]
pub fn parse_keys(notation: &str) -> Vec<KeyEvent> {
    let chars: Vec<char> = notation.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '<' {
            if let Some(len) = chars[i + 1..].iter().position(|&c| c == '>') {
                // `<C->>`-style names never occur; a name is at least one char.
                let name: String = chars[i + 1..i + 1 + len].iter().collect();
                if let Some(k) = named_key(&name) {
                    out.push(k);
                    i += len + 2;
                    continue;
                }
                // `<M->>`: the first `>` is the key itself, the second closes the name.
                if name.ends_with('-') && chars.get(i + len + 2) == Some(&'>') {
                    if let Some(k) = named_key(&format!("{name}>")) {
                        out.push(k);
                        i += len + 3;
                        continue;
                    }
                }
            }
        }
        out.push(plain(KeyCode::Char(chars[i])));
        i += 1;
    }
    out
}

/// A key with the modifiers crossterm reports for it unmodified: `SHIFT` on an uppercase letter.
fn plain(code: KeyCode) -> KeyEvent {
    let mods = match code {
        KeyCode::Char(c) if c.is_uppercase() => KeyModifiers::SHIFT,
        _ => KeyModifiers::NONE,
    };
    KeyEvent::new(code, mods)
}

/// The base key of a bracketed name (`Esc`, `CR`, …), unmodified. A single char is a base key only
/// under a modifier (`<C-w>`): a bare `<a>` is not a key name, so it stays literal text.
fn base_key(name: &str, modified: bool) -> Option<KeyEvent> {
    let mut it = name.chars();
    if let (Some(c), None) = (it.next(), it.next()) {
        return modified.then(|| plain(KeyCode::Char(c)));
    }
    let code = match name.to_ascii_lowercase().as_str() {
        "esc" => KeyCode::Esc,
        "cr" | "enter" | "return" => KeyCode::Enter,
        "bs" => KeyCode::Backspace,
        "tab" => KeyCode::Tab,
        "space" => KeyCode::Char(' '),
        "lt" => KeyCode::Char('<'),
        "del" => KeyCode::Delete,
        "up" => KeyCode::Up,
        "down" => KeyCode::Down,
        "left" => KeyCode::Left,
        "right" => KeyCode::Right,
        "home" => KeyCode::Home,
        "end" => KeyCode::End,
        "insert" => KeyCode::Insert,
        "pageup" => KeyCode::PageUp,
        "pagedown" => KeyCode::PageDown,
        _ => return None,
    };
    Some(plain(code))
}

/// A bracketed name, with any `C-`/`M-`/`A-`/`S-` prefixes applied as a legacy terminal encodes them
/// (`S-` only on the special keys, which xterm reports with a modifier parameter: `<S-Up>`, `<S-CR>`).
fn named_key(name: &str) -> Option<KeyEvent> {
    let (mut ctrl, mut meta, mut shift, mut rest) = (false, false, false, name);
    loop {
        let lower = rest.get(..2).map(str::to_ascii_lowercase);
        match lower.as_deref() {
            Some("c-") if rest.len() > 2 => ctrl = true,
            Some("m-" | "a-") if rest.len() > 2 => meta = true,
            // Only a special key carries SHIFT this way; `<S-a>` stays unknown (a literal `<`).
            Some("s-") if rest.len() > 3 => shift = true,
            _ => break,
        }
        rest = &rest[2..];
    }
    let base = base_key(rest, ctrl || meta)?;
    let base = if shift && base.code == KeyCode::Tab {
        // A terminal sends `<S-Tab>` as `CSI Z`, which crossterm reports as BackTab.
        KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT)
    } else if shift {
        KeyEvent::new(base.code, base.modifiers | KeyModifiers::SHIFT)
    } else {
        base
    };
    let key = if ctrl { control(base, name) } else { base };
    Some(if meta {
        KeyEvent::new(key.code, key.modifiers | KeyModifiers::ALT)
    } else {
        key
    })
}

/// `CTRL-` applied to `base` the way a legacy terminal encodes it and crossterm decodes the byte.
fn control(base: KeyEvent, name: &str) -> KeyEvent {
    let ctrl = |c: char| KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL);
    match base.code {
        KeyCode::Char(c) if c.is_ascii_alphabetic() => match c.to_ascii_lowercase() {
            'i' => plain(KeyCode::Tab),
            'm' => plain(KeyCode::Enter),
            l => ctrl(l),
        },
        KeyCode::Char(' ' | '@') => ctrl(' '),
        KeyCode::Char('[') => plain(KeyCode::Esc),
        KeyCode::Char('\\') => ctrl('4'),
        KeyCode::Char(']') => ctrl('5'),
        KeyCode::Char('^') => ctrl('6'),
        KeyCode::Char('_') => ctrl('7'),
        // xterm reports the modified cursor / editing keys with a modifier parameter (`CSI 1;5H`).
        KeyCode::Up
        | KeyCode::Down
        | KeyCode::Left
        | KeyCode::Right
        | KeyCode::Home
        | KeyCode::End
        | KeyCode::PageUp
        | KeyCode::PageDown => KeyEvent::new(base.code, base.modifiers | KeyModifiers::CONTROL),
        _ => panic!("<{name}> has no legacy terminal encoding"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn k(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, mods)
    }

    /// A macro replay's edits are recorded as `Macro`, typed ones, `.` and `:normal` as `UserInput`
    /// (INV-ORIGIN, #880), and the origin goes back to `UserInput` after the replay.
    #[test]
    fn macro_replay_edits_carry_the_macro_origin() {
        use ruse_core::TransactionOrigin::{Macro, UserInput};
        let mut h = Headless::vim("abcdefgh\n");
        h.keys("qaxq@a.:normal x<CR>2@a");
        assert_eq!(h.text(), "gh\n");
        let origins: Vec<_> = h
            .workspace()
            .focused()
            .doc
            .history()
            .creation_order()
            .into_iter()
            .filter_map(|(_, o, _)| o)
            .collect();
        assert_eq!(
            origins,
            [UserInput, Macro, UserInput, UserInput, Macro, Macro]
        );
        assert_eq!(h.workspace().origin(), UserInput);
    }

    /// #1182: a window on another, unchanged buffer keeps that buffer's line index across frames: one
    /// newline scan, not one per frame (it used to rebuild it in every viewport pass and every render).
    #[test]
    fn an_unfocused_pane_reuses_its_buffers_line_index() {
        let big: String = (0..200_000).map(|i| format!("line {i}\n")).collect();
        let mut h = Headless::vim(&big);
        h.keys(":vsplit<CR>:enew<CR>");
        let ws = h.workspace();
        let pane = (0..ws.window_count())
            .find(|&i| ws.pane(i).view.doc() != ws.focused_buffer())
            .expect("two buffers shown");
        let other = ws.pane(pane).view.doc();
        let builds = |h: &Headless| {
            let rev = h.s.ws.pane(pane).doc.revision();
            h.s.pane_idx.get(other, rev).map(|ix| ix.builds())
        };
        assert_eq!(builds(&h), Some(1));
        h.keys("ihello<Esc>").keys(&"hl".repeat(100));
        assert_eq!(builds(&h), Some(1), "no rebuild over 206 frames");
        // An edit there, made from the other window, is one rebuild when it is shown unfocused again.
        h.keys("<C-w>wx<C-w>w").keys("hlhl");
        assert_eq!(builds(&h), Some(1));
    }

    #[test]
    fn emsg_code_reads_only_a_numbered_error() {
        assert_eq!(emsg_code("E486: pattern not found: x"), Some("E486"));
        assert_eq!(emsg_code("E: bad pattern"), None);
        assert_eq!(emsg_code("Error"), None);
        assert_eq!(emsg_code("E12 no colon"), None);
        assert_eq!(emsg_code(""), None);
    }

    #[test]
    fn notation_decodes_like_a_legacy_terminal() {
        let none = KeyModifiers::NONE;
        let ctrl = KeyModifiers::CONTROL;
        assert_eq!(
            parse_keys("aZ<Esc><cr><C-w><C-\\><M-x><M-<><M->><lt><BS><C-Space>"),
            vec![
                k(KeyCode::Char('a'), none),
                k(KeyCode::Char('Z'), KeyModifiers::SHIFT),
                k(KeyCode::Esc, none),
                k(KeyCode::Enter, none),
                k(KeyCode::Char('w'), ctrl),
                k(KeyCode::Char('4'), ctrl),
                k(KeyCode::Char('x'), KeyModifiers::ALT),
                k(KeyCode::Char('<'), KeyModifiers::ALT),
                k(KeyCode::Char('>'), KeyModifiers::ALT),
                k(KeyCode::Char('<'), none),
                k(KeyCode::Backspace, none),
                k(KeyCode::Char(' '), ctrl),
            ]
        );
        assert_eq!(
            parse_keys("<M-BS><C-i><x<a>"),
            vec![
                k(KeyCode::Backspace, KeyModifiers::ALT),
                k(KeyCode::Tab, none),
                k(KeyCode::Char('<'), none),
                k(KeyCode::Char('x'), none),
                k(KeyCode::Char('<'), none),
                k(KeyCode::Char('a'), none),
                k(KeyCode::Char('>'), none),
            ]
        );
    }

    #[test]
    fn a_key_reaches_the_buffer_through_the_session() {
        let mut h = Headless::vim("one\ntwo\nthree\n");
        h.keys("jdd");
        assert_eq!(h.text(), "one\nthree\n");
        assert_eq!(h.mode(), Mode::Normal);
        h.keys("ix<Esc>");
        assert_eq!(h.text(), "one\nxthree\n");
        assert_eq!(h.cursor_row_col(), (2, 0));
    }

    #[test]
    fn a_macro_replays_through_the_session_a_frame_per_key() {
        let mut h = Headless::vim("a\nb\nc\n");
        h.keys("qqA!<Esc>jq2@q");
        assert_eq!(h.text(), "a!\nb!\nc!\n");
    }

    #[test]
    fn quit_stops_the_session() {
        let mut h = Headless::vim("x\n");
        h.keys(":q<CR>");
        assert!(h.quit());
        h.keys("dd");
        assert_eq!(h.text(), "x\n", "keys after quit are ignored");
    }
}
