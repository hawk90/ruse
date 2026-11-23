//! The command-line key paths (#1356): the `:`/`/`/`?` line and its wildmenu, the command-line window,
//! the `M-x` and expression prompts, history recall, and the search a `/`/`?` line submits (`n`/`N`
//! repeat it). Moved out of `input/mod.rs` as an `impl InputEngine` block; the line editor itself lives
//! on [`CmdLine`].

use super::{
    cmdline, complete, emacs, emacs_command_by_name, history, is_chord, plain, CmdLine, CmdWin,
    Command, ExprTarget, Feed, InputEngine, KeyCode, KeyEvent, KeyModifiers, SearchOffset,
    SearchOp, SuspendedCmdline, WildKey,
};

impl InputEngine {
    /// The active command-line as `(prefix, text, cursor)` for the frontend to render — `None` when
    /// not on the command line. The frontend reads this instead of owning any line buffer (F-026).
    #[must_use]
    pub fn cmdline(&self) -> Option<(char, &str, usize)> {
        self.cmdline
            .as_ref()
            .map(|c| (c.prefix, c.buffer.as_str(), c.cursor))
    }

    /// Splice `text` into the open command line at the caret — the frontend's response to
    /// [`Feed::CmdlineInsertUnder`] / [`Feed::CmdlineInsertLine`], after it has resolved the text from the
    /// buffer. No-op when no line is open; splicing also ends any history-recall walk.
    pub fn cmdline_splice(&mut self, text: &str) {
        if let Some(cl) = self.cmdline.as_mut() {
            cl.insert(text);
        }
    }

    /// Splice a register's text at the caret — the frontend's response to [`Feed::CmdlineInsertRegister`].
    /// As in nvim, a linewise register drops its final newline and every remaining newline is typed as a
    /// literal `^M` (`:<C-r>a` on `"l1\nl2\n"` gives `l1^Ml2`).
    pub fn cmdline_splice_register(&mut self, text: &str, linewise: bool) {
        let text = if linewise {
            text.strip_suffix('\n').unwrap_or(text)
        } else {
            text
        };
        self.cmdline_splice(&text.replace('\n', "\r"));
    }

    /// The completion sources, to register another ([`complete::Completer::register`]).
    pub fn completer_mut(&mut self) -> &mut complete::Completer {
        &mut self.completer
    }

    /// Run completion key `key` on the open `:` line — the frontend's answer to [`Feed::CmdlineComplete`],
    /// with `ctx` for what lies outside the line. nvim v0.12.6 defaults (`wildmode=full`,
    /// `wildoptions=pum`): one match is inserted; several open the wildmenu on the first (`<S-Tab>`: the
    /// last); none leaves the line as it is. Where nothing completes (`:s/a/`), `<Tab>` types a literal
    /// `^I`.
    pub fn cmdline_complete(&mut self, key: WildKey, ctx: &dyn complete::CompletionContext) {
        let Some(cl) = self.cmdline.as_mut() else {
            return;
        };
        let before = cl.before_cursor().to_string();
        let Some(c) = self.completer.complete(&before, ctx) else {
            if key == WildKey::Next {
                cl.insert("\t");
            }
            return;
        };
        let mut items = c.matches.items;
        let word = &before[c.start..];
        match key {
            WildKey::List => cl.listing = (!items.is_empty()).then_some(items),
            WildKey::Longest => {
                let longest = complete::longest_common(&items, c.files);
                if longest.len() > word.len() {
                    cl.replace_word(c.start, &longest);
                }
            }
            WildKey::All if !items.is_empty() => {
                let all: Vec<&str> = items.iter().map(|i| i.text.as_str()).collect();
                cl.replace_word(c.start, &all.join(" "));
            }
            WildKey::All => {}
            WildKey::Next | WildKey::Prev => {
                cl.listing = None;
                match items.len() {
                    0 => {}
                    1 => cl.replace_word(c.start, &items.remove(0).text),
                    n => {
                        let first = if key == WildKey::Next { 0 } else { n - 1 };
                        cl.start_wild(c.start, items, c.matches.pum_at, c.files, first);
                    }
                }
            }
        }
    }

    /// The open wildmenu for the popup menu: its entries, the selected one, and the char column of the
    /// line where the entries' text aligns. `None` when no wildmenu is open.
    #[must_use]
    pub fn cmdline_wild(&self) -> Option<(Vec<&str>, Option<usize>, usize)> {
        let w = self.cmdline.as_ref()?.wild.as_ref()?;
        Some((
            w.items.iter().map(|i| i.pum.as_str()).collect(),
            w.selected,
            w.pum_char_col(),
        ))
    }

    /// The matches `c_CTRL-D` listed on the open line, for the frontend to lay out in columns
    /// ([`complete::list_columns`]).
    #[must_use]
    pub fn cmdline_listing(&self) -> Option<&[complete::Item]> {
        self.cmdline.as_ref()?.listing.as_deref()
    }

    /// Keys on an open wildmenu (`:help wildmenu`): `<Tab>`/`CTRL-N`/`<Right>` and `<S-Tab>`/`CTRL-P`/
    /// `<Left>` cycle, `CTRL-E` restores the typed text, `CTRL-Y` keeps the match. `<Up>`/`<Down>` keep it
    /// too; on file names `<Down>` descends into a selected directory and `<Up>` completes the parent
    /// directory (nvim v0.12.6). `<Esc>` abandons the line as ever; any other key keeps the match and then
    /// acts. `None` when the key is not the wildmenu's to take.
    fn feed_wild(&mut self, key: KeyEvent) -> Option<Feed> {
        let cl = self.cmdline.as_mut()?;
        cl.wild.as_ref()?;
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        match key.code {
            KeyCode::BackTab => cl.wild_step(false),
            KeyCode::Tab => cl.wild_step(!shift),
            KeyCode::Right => cl.wild_step(true),
            KeyCode::Left => cl.wild_step(false),
            KeyCode::Char('n') if ctrl => cl.wild_step(true),
            KeyCode::Char('p') if ctrl => cl.wild_step(false),
            KeyCode::Char('e') if ctrl => cl.wild_cancel(),
            KeyCode::Char('y') if ctrl => cl.wild = None,
            KeyCode::PageUp | KeyCode::PageDown => cl.wild = None,
            KeyCode::Up | KeyCode::Down => {
                let files = cl.wild.as_ref().is_some_and(|w| w.files);
                let word = cl.wild_word().map(|(s, w)| (s, w.to_string()));
                cl.wild = None;
                let (Some((start, word)), true) = (word, files) else {
                    return Some(Feed::Pending);
                };
                if key.code == KeyCode::Down {
                    return Some(if word.ends_with('/') {
                        Feed::CmdlineComplete(WildKey::Next)
                    } else {
                        Feed::Pending
                    });
                }
                cl.replace_word(start, &complete::parent_word(&word));
                return Some(Feed::CmdlineComplete(WildKey::Next));
            }
            KeyCode::Esc => return None,
            _ => {
                cl.wild = None;
                return None;
            }
        }
        Some(Feed::Pending)
    }

    /// Set aside the open command line (a `gQ` Ex-mode line that stays open while its command runs), so
    /// keys a `:normal` feeds reach Normal mode instead of the line (#807). [`Self::resume_cmdline`] puts
    /// it back.
    pub fn suspend_cmdline(&mut self) -> SuspendedCmdline {
        SuspendedCmdline(self.cmdline.take())
    }

    /// Restore a command line [`Self::suspend_cmdline`] set aside, replacing any line left open since.
    pub fn resume_cmdline(&mut self, saved: SuspendedCmdline) {
        self.cmdline = saved.0;
    }

    /// Open the command-line namespace with `prefix` (`:`/`/`), optionally as `gQ` Ex mode.
    pub(super) fn open_cmdline(&mut self, prefix: char, ex_mode: bool) {
        self.cmdline = Some(CmdLine {
            ex_mode,
            // A search line follows the Insert language state; a `:` line never starts mapped (#810).
            lang: matches!(prefix, '/' | '?') && self.lang_active,
            ..CmdLine::new(prefix)
        });
    }

    /// Open the `:` command-line PRE-SEEDED with `seed` and the caret at its end — the landing point of the
    /// Normal-mode `!{motion}` / `!!` filter operator. The frontend resolves the motion's line range and
    /// seeds `"{first},{last}!"`, so typing the shell command and pressing `<CR>` runs the SAME
    /// `:{range}!{cmd}` ex filter (reusing its executor + `shell.rs`). Vim shows this pre-filled `:.,.+N!`.
    pub fn open_filter_cmdline(&mut self, seed: &str) {
        let mut cl = CmdLine::new(':');
        cl.set_text(seed.to_string());
        self.cmdline = Some(cl);
    }

    /// Open the command-line window (`:help cmdwin`) for `kind` — `:` mirrors the ex ring, `/`/`?` the
    /// shared search ring. The frontend calls this when `q:`/`q/`/`q?` is recognised (the macro layer routes
    /// the `q`-prefixed `:`/`/`/`?` here instead of arming a macro recording). No-op if already open.
    pub fn open_cmdwin(&mut self, kind: char) {
        // The window opens from a clean Normal base: no operator/count from an earlier search line applies.
        self.pending_search = None;
        let ring = if kind == '/' || kind == '?' {
            &self.search_history
        } else {
            &self.ex_history
        };
        self.cmdwin = Some(CmdWin::open(kind, ring.entries_ref()));
    }

    /// The open command-line window's kind glyph (`:`/`/`/`?`), or `None` when closed. The frontend reads it
    /// for the prompt glyph and to route keys / suppress Normal-mode intercepts while the overlay owns them.
    #[must_use]
    pub fn cmdwin(&self) -> Option<char> {
        self.cmdwin.as_ref().map(|c| c.kind)
    }

    /// The command-line window's visible list rows `(text, is_selected)` for the overlay paint slot, or an
    /// empty vec when closed. The frontend paints these in the same slot the pickers use.
    #[must_use]
    pub fn cmdwin_rows(&self) -> Vec<(String, bool)> {
        self.cmdwin.as_ref().map(CmdWin::rows).unwrap_or_default()
    }

    /// Route one key into the open command-line window (`:help cmdwin`). `j`/`<Down>` and `k`/`<Up>` move the
    /// selection; `<CR>` runs the selected line — an ex line becomes [`Feed::ExecuteEx`], a search folds
    /// through [`Self::submit_search`] — and closes the window (an empty line runs nothing); `<Esc>`/`<C-c>`
    /// closes without running. Any other key is swallowed (the overlay is modal). Accepting a line records it
    /// in the matching ring, exactly as the `:`/`/` prompt does on `<CR>`.
    pub(super) fn feed_cmdwin(&mut self, key: KeyEvent) -> Feed {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let Some(cw) = self.cmdwin.as_mut() else {
            return Feed::Ignored;
        };
        match key.code {
            KeyCode::Char('c') if ctrl => {
                self.cmdwin = None;
                Feed::Ignored
            }
            KeyCode::Esc => {
                self.cmdwin = None;
                Feed::Ignored
            }
            KeyCode::Char('j') | KeyCode::Down => {
                cw.down();
                Feed::Pending
            }
            KeyCode::Char('k') | KeyCode::Up => {
                cw.up();
                Feed::Pending
            }
            KeyCode::Char('G') => {
                // Jump to the empty last line (Vim `G`); the list is short, so no count handling.
                while {
                    let before = cw.selected_line().to_string();
                    cw.down();
                    before != cw.selected_line()
                } {}
                Feed::Pending
            }
            KeyCode::Enter => {
                let kind = cw.kind;
                let text = cw.selected_line().to_string();
                self.cmdwin = None;
                if text.is_empty() {
                    return Feed::Ignored; // `<CR>` on the empty line just closes (nothing to run)
                }
                // Run the chosen line through the SAME accept path the `:`/`/` prompt uses on `<CR>`: push it
                // onto the matching ring, then execute (`/`+`?` share the search ring; `:` the ex ring).
                if kind == '/' || kind == '?' {
                    self.search_history.push(&text);
                    self.submit_search(text, kind == '?')
                } else {
                    self.ex_history.push(&text);
                    Feed::ExecuteEx(text)
                }
            }
            _ => Feed::Pending, // modal: swallow everything else
        }
    }

    /// Open the Emacs `M-x` minibuffer (F-012): the same command-line namespace, reading a command NAME.
    /// The prompt glyph is a placeholder here; the frontend shows the `M-x ` prompt (a rendering follow-up).
    pub(super) fn open_minibuffer(&mut self) {
        self.cmdline = Some(CmdLine {
            mx: true,
            ..CmdLine::new(':')
        });
    }

    /// Open the expression-register prompt (`"=` / `<C-r>=`, `:help quote=`): the same command-line namespace,
    /// reading an EXPRESSION. The `=` prefix renders the prompt glyph. On `<CR>` the collected string is handed
    /// to the evaluator via the command `target` produces (see [`ExprTarget`]); `<Esc>` aborts.
    pub(super) fn open_expr_prompt(&mut self, target: ExprTarget) {
        self.cmdline = Some(CmdLine {
            expr: Some(target),
            ..CmdLine::new('=')
        });
    }

    /// Route a key into the open command-line buffer (F-026). The namespace owns the buffer and its caret
    /// (`:help cmdline-editing`): a printable key types at the caret, `<Left>`/`<Right>`/`<Home>`/`<End>`
    /// and the word motions move it, `<BS>`/`<Del>`/`CTRL-U`/`CTRL-W` delete around it, `<Esc>` aborts, and
    /// `<CR>` finalises — a search folds through [`Self::submit_search`], an ex line becomes
    /// [`Feed::ExecuteEx`]. In `gQ` Ex mode the line re-opens after `<CR>` until `:visual`/`:vi`/an empty
    /// line exits.
    pub(super) fn feed_cmdline(&mut self, mut key: KeyEvent) -> Feed {
        // `c_CTRL-V`/`c_CTRL-Q`: the collector owns the key; a code ended by a non-digit hands that key
        // back to be processed as ordinary input.
        if let Some(cl) = self.cmdline.as_mut().filter(|cl| cl.literal.is_some()) {
            match cl.feed_literal(key) {
                cmdline::LiteralStep::Reprocess(k) => key = k,
                _ => return Feed::Pending,
            }
        }
        if let Some(feed) = self.cmdline_special_key(key) {
            return feed;
        }
        self.cmdline_edit(key)
    }

    /// The keys that act before the line editor: the `c_CTRL-K` digraph collector, the `c_CTRL-R`
    /// prefix, the wildmenu, completion and history recall. `None` when the key is the editor's.
    fn cmdline_special_key(&mut self, key: KeyEvent) -> Option<Feed> {
        let Some(cl) = self.cmdline.as_mut() else {
            return Some(Feed::Ignored);
        };
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        // `c_CTRL-K {char1} {char2}`: the digraph collector owns the next two keys.
        if cl.digraph.is_some() {
            cl.feed_digraph(key);
            return Some(Feed::Pending);
        }
        // `c_CTRL-R` prefix armed: THIS key selects what to splice at the caret. Checked before the
        // history-recall match below so that `C-r C-p` is consumed as a selector, not as `<C-p>` recall.
        // `CTRL-W`/`CTRL-A`/`CTRL-L` name the word/WORD/line under the cursor; `CTRL-R`/`CTRL-O` (insert
        // literally) keep the prefix armed for the register name; a plain char names a register, which the
        // frontend reads (the engine has none). Any other key aborts the prefix and stays on the line.
        if cl.ctrl_r {
            cl.ctrl_r = false;
            return Some(match key.code {
                KeyCode::Char('w') if ctrl => Feed::CmdlineInsertUnder { big: false },
                KeyCode::Char('a') if ctrl => Feed::CmdlineInsertUnder { big: true },
                KeyCode::Char('l') if ctrl => Feed::CmdlineInsertLine,
                KeyCode::Char('r' | 'o') if ctrl => {
                    cl.ctrl_r = true;
                    Feed::Pending
                }
                // `C-r =` (the expression register's prompt) is not wired on the command line yet.
                KeyCode::Char('=') => Feed::Pending,
                KeyCode::Char(c) if !is_chord(key) => Feed::CmdlineInsertRegister(c),
                _ => Feed::Pending,
            });
        }
        // The wildmenu takes its keys first (`CTRL-N`/`CTRL-P` cycle there instead of recalling history).
        if cl.wild.is_some() {
            if let Some(feed) = self.feed_wild(key) {
                return Some(feed);
            }
        }
        let Some(cl) = self.cmdline.as_mut() else {
            return Some(Feed::Ignored);
        };
        // Completion keys on the `:` line (`:help cmdline-completion`); the `/` line completes nothing
        // (nvim v0.12.6 neither inserts nor completes a `<Tab>` there).
        if cl.prefix == ':' && !cl.mx && cl.expr.is_none() {
            let wild_key = match key.code {
                KeyCode::BackTab => Some(WildKey::Prev),
                KeyCode::Tab if key.modifiers.contains(KeyModifiers::SHIFT) => Some(WildKey::Prev),
                KeyCode::Tab => Some(WildKey::Next),
                KeyCode::Char('i') if ctrl => Some(WildKey::Next),
                KeyCode::Char('d') if ctrl => Some(WildKey::List),
                KeyCode::Char('l') if ctrl => Some(WildKey::Longest),
                KeyCode::Char('a') if ctrl => Some(WildKey::All),
                _ => None,
            };
            if let Some(k) = wild_key {
                return Some(Feed::CmdlineComplete(k));
            }
        }
        if ctrl && key.code == KeyCode::Char('r') && !cl.mx {
            cl.ctrl_r = true;
            return Some(Feed::Pending);
        }
        // History recall (`:help cmdline-history`). nvim-VERIFIED distinction: `<Up>`/`<Down>` recall
        // PREFIX-FILTERED by the text before the caret; `<C-p>`/`<C-n>`, `<S-Up>`/`<S-Down>` and
        // `<PageUp>`/`<PageDown>` walk the RAW ring unfiltered.
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        match key.code {
            KeyCode::Up if !shift => return Some(self.cmdline_recall(true, true)),
            KeyCode::Down if !shift => return Some(self.cmdline_recall(false, true)),
            KeyCode::Up | KeyCode::PageUp => return Some(self.cmdline_recall(true, false)),
            KeyCode::Down | KeyCode::PageDown => return Some(self.cmdline_recall(false, false)),
            KeyCode::Char('p') if ctrl => return Some(self.cmdline_recall(true, false)),
            KeyCode::Char('n') if ctrl => return Some(self.cmdline_recall(false, false)),
            _ => {}
        }
        None
    }

    /// The line editor proper (`:help cmdline-editing`): the CTRL aliases, caret motion, deletion, typing
    /// and `<CR>`.
    fn cmdline_edit(&mut self, mut key: KeyEvent) -> Feed {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let Some(cl) = self.cmdline.as_mut() else {
            return Feed::Ignored;
        };
        // CTRL aliases (nvim v0.12.6): `c_CTRL-C` and `<C-[>` abandon the line like `<Esc>`, `c_CTRL-H` is
        // `<BS>`, `c_CTRL-J`/`<C-m>` run it like `<CR>`, `c_CTRL-B`/`c_CTRL-E` are `<Home>`/`<End>`. The
        // Emacs `M-x` minibuffer shares this editor with Emacs keys instead: `C-g` quits, `C-a`/`C-e`/`C-b`/
        // `C-f` move, `C-d` deletes forward, `C-k` kills to the end; `C-c`/`C-h`/`C-u`/`C-w` are inert.
        // `c_CTRL-^` flips the language map for this line; on a `/`/`?` line it is the Insert state too
        // (`'imsearch'` -1), so the next search and Insert keep it (nvim `command_line_toggle_langmap`, #810).
        if ctrl && !cl.mx && matches!(key.code, KeyCode::Char('^' | '6')) {
            cl.lang = !cl.lang;
            if matches!(cl.prefix, '/' | '?') && cl.expr.is_none() {
                self.lang_active = cl.lang;
            }
            return Feed::Pending;
        }
        key = match key.code {
            KeyCode::Char('[') if ctrl => plain(KeyCode::Esc),
            KeyCode::Char('c') if ctrl && !cl.mx => plain(KeyCode::Esc),
            KeyCode::Char('g') if ctrl && cl.mx => plain(KeyCode::Esc),
            KeyCode::Char('h') if ctrl && !cl.mx => plain(KeyCode::Backspace),
            KeyCode::Char('j' | 'm') if ctrl => plain(KeyCode::Enter),
            KeyCode::Char('b') if ctrl && !cl.mx => plain(KeyCode::Home),
            KeyCode::Char('e') if ctrl => plain(KeyCode::End),
            KeyCode::Char('a') if ctrl && cl.mx => plain(KeyCode::Home),
            KeyCode::Char('b') if ctrl && cl.mx => plain(KeyCode::Left),
            KeyCode::Char('f') if ctrl && cl.mx => plain(KeyCode::Right),
            _ => key,
        };
        // `<S-Left>`/`<C-Left>` and `<S-Right>`/`<C-Right>` move by space-separated WORD.
        let word = key
            .modifiers
            .intersects(KeyModifiers::SHIFT | KeyModifiers::CONTROL);
        // An empty Vim line (not `M-x`, not `gQ` Ex mode) is abandoned by `<BS>`/`<Del>` (#811).
        let cancels_empty = !cl.mx && !cl.ex_mode;
        match key.code {
            KeyCode::Esc => {
                self.cmdline = None;
                // Abandoning a `/` line abandons the operator/count it carried (`d/<Esc>`): left behind, a
                // later `q/` window search would fold into it and delete (#798).
                self.pending_search = None;
                Feed::Ignored
            }
            KeyCode::Backspace => {
                if cl.backspace() && cancels_empty {
                    return self.feed_cmdline(plain(KeyCode::Esc));
                }
                Feed::Pending
            }
            KeyCode::Delete => {
                if cl.delete() && cancels_empty {
                    return self.feed_cmdline(plain(KeyCode::Esc));
                }
                Feed::Pending
            }
            KeyCode::Char('d') if ctrl && cl.mx => {
                if cl.cursor < cl.buffer.chars().count() {
                    cl.delete();
                }
                Feed::Pending
            }
            KeyCode::Char('k') if ctrl && cl.mx => {
                cl.kill_after();
                Feed::Pending
            }
            KeyCode::Left if word => {
                cl.word_left();
                Feed::Pending
            }
            KeyCode::Right if word => {
                cl.word_right();
                Feed::Pending
            }
            KeyCode::Left => {
                cl.left();
                Feed::Pending
            }
            KeyCode::Right => {
                cl.right();
                Feed::Pending
            }
            KeyCode::Home => {
                cl.home();
                Feed::Pending
            }
            KeyCode::End => {
                cl.end();
                Feed::Pending
            }
            // `c_<Insert>` toggles overstrike.
            KeyCode::Insert => {
                cl.overstrike = !cl.overstrike;
                Feed::Pending
            }
            // `c_CTRL-U` — delete everything before the caret.
            KeyCode::Char('u') if ctrl && !cl.mx => {
                cl.kill_before();
                Feed::Pending
            }
            // `c_CTRL-W` — delete the word before the caret.
            KeyCode::Char('w') if ctrl && !cl.mx => {
                cl.kill_word_before();
                Feed::Pending
            }
            // `c_CTRL-V` / `c_CTRL-Q` — insert the next key literally (`<C-v><CR>` types a `^M`), or a
            // numeric code.
            KeyCode::Char('v' | 'q') if ctrl => {
                cl.literal = Some(cmdline::Literal::AwaitFirst);
                Feed::Pending
            }
            // `c_CTRL-K {char1} {char2}` — a digraph (Emacs `C-k` is kill-line, above).
            KeyCode::Char('k') if ctrl => {
                cl.digraph = Some(None);
                Feed::Pending
            }
            // Any other chord is unbound here and never appends its letter (#599). nvim appends some of them
            // (`<C-g>`, `<C-o>`, `<C-t>`, …) as a raw control character; ruse appends nothing.
            KeyCode::Char(_) if is_chord(key) => Feed::Pending,
            KeyCode::Char(c) => {
                cl.insert(c.encode_utf8(&mut [0; 4]));
                Feed::Pending
            }
            KeyCode::Enter => self.cmdline_accept(),
            _ => Feed::Pending,
        }
    }

    /// `<CR>` on the open line: evaluate an expression prompt, run an `M-x` name, run a `gQ` Ex line and
    /// re-prompt, or close the line and submit its search / Ex command, recording it in its history.
    fn cmdline_accept(&mut self) -> Feed {
        let Some(cl) = self.cmdline.as_mut() else {
            return Feed::Ignored;
        };
        let prefix = cl.prefix;
        let ex_mode = cl.ex_mode;
        let mx = cl.mx;
        let expr = cl.expr;
        let text = std::mem::take(&mut cl.buffer);
        // Expression-register prompt (`"=` / `<C-r>=`): hand the collected expression to the editor,
        // which evaluates it. `Paste` arms the `"=` register for the next paste; `Insert` splices the
        // result at the caret. A malformed expression degrades in the editor (empty result).
        if let Some(target) = expr {
            self.cmdline = None;
            // An empty expression is the last one again (nvim's `expr_line`, #1342).
            let text = self.take_expr_line(text);
            return match target {
                ExprTarget::Paste => Feed::Cmd(Command::SetExprRegister(text)),
                ExprTarget::Insert => Feed::Cmd(Command::InsertEval(text)),
            };
        }
        if mx {
            // `M-x <name> <CR>`: resolve the command name against the registry; an unknown name is a
            // no-op (Emacs shows "[No match]"). Completion / history are deferred (F-004).
            self.cmdline = None;
            if let Some(a) = emacs::emacs_action_by_name(&text) {
                return Feed::EmacsAction(a);
            }
            return match emacs_command_by_name(&text) {
                Some(cmd) => Feed::Cmd(cmd),
                None => Feed::Ignored,
            };
        }
        if ex_mode {
            // Ex mode: `:visual`/`:vi`/empty leaves it; anything else runs and re-prompts.
            if text.is_empty() || text == "visual" || text == "vi" {
                self.cmdline = None;
                return Feed::Ignored;
            }
            // A gQ-Ex line is still an ex command: record it in the `:` history (empty ignored by
            // push). gQ stays in the SAME CmdLine, so reset its caret and walk for the next line.
            self.ex_history.push(&text);
            cl.cursor = 0;
            cl.overstrike = false;
            cl.walk = history::HistWalk::default();
            return Feed::ExecuteEx(text);
        }
        self.cmdline = None;
        // Push the ACCEPTED line onto the matching ring (`:help cmdline-history`): `/`+`?` share the
        // search ring, `:` uses the ex ring; empty lines are ignored by `push`. Kept separate.
        if prefix == '/' || prefix == '?' {
            self.search_history.push(&text);
            self.submit_search(text, prefix == '?')
        } else {
            self.ex_history.push(&text);
            Feed::ExecuteEx(text)
        }
    }

    /// Recall a history entry into the open command-line (`:help cmdline-history`). `prev` walks older
    /// (`<Up>`/`<C-p>`), else more-recent (`<Down>`/`<C-n>`); `filter` prefix-filters by the text before
    /// the caret (`<Up>`/`<Down>`) vs the raw ring (`<C-p>`/`<C-n>`). Chooses the ex vs search ring by
    /// prefix; the `M-x`/expression prompts have no history and pass the key through. Rewrites the buffer
    /// and puts the caret at its end.
    fn cmdline_recall(&mut self, prev: bool, filter: bool) -> Feed {
        let Some(cl) = self.cmdline.as_mut() else {
            return Feed::Ignored;
        };
        // Only the Vim `:` ex line and the `/`+`?` search line carry history. The `M-x` minibuffer and the
        // expression-register prompt do not (their own histories are separate follow-ups) — pass through.
        if cl.mx || cl.expr.is_some() {
            return Feed::Pending;
        }
        // Disjoint borrows: `cl` borrows `self.cmdline`; the ring borrows a separate field.
        let ring = if cl.prefix == '/' || cl.prefix == '?' {
            &self.search_history
        } else {
            &self.ex_history
        };
        let current = cl.before_cursor().to_string();
        let recalled = if prev {
            ring.recall_prev(&mut cl.walk, &current, filter)
        } else {
            ring.recall_next(&mut cl.walk, &current, filter)
        };
        if let Some(text) = recalled {
            cl.set_text(text);
        }
        Feed::Pending
    }

    /// Open the search command-line for `/` (forward) or `?` (backward). Search is a MOTION, so an armed
    /// operator/count must survive the minibuffer (`d/pat`, `2?pat`): capture them (op-count × pending
    /// count, as `motion()` folds) BEFORE `reset()` clears the axes, then hand off to the command-line
    /// namespace, which collects the pattern and calls [`Self::submit_search`] to build the command.
    pub(super) fn enter_search(&mut self, backward: bool) -> Feed {
        // `!/pat` keeps the filter through the minibuffer: `submit_search` hands the span to the frontend.
        self.pending_search = Some((self.search_op(), self.mcount(), self.filter_armed()));
        self.reset();
        self.open_cmdline(if backward { '?' } else { '/' }, false);
        Feed::Pending
    }

    /// Complete a `/pattern` (forward) or `?pattern` (backward) line, called by the command-line namespace
    /// on `<CR>` with `backward` from the prefix. Folds the pattern into the operator/count captured when
    /// `/`/`?` was pressed and yields the finished [`Command::Search`]: bare (`?pat`, `2?pat`) moves, or
    /// `d?pat`/`c?pat`/`y?pat` operates over the exclusive span between cursor and match. Records
    /// `(pattern, direction)` for `n`/`N`. An EMPTY pattern reuses the last search pattern in the CURRENT
    /// direction (Vim's `?<CR>` / `/<CR>`); with no prior search it aborts, dropping the armed operator.
    pub fn submit_search(&mut self, line: String, backward: bool) -> Feed {
        self.cmdline = None; // completing a search closes the command-line namespace (F-026)
        let (op, count, filter) = self
            .pending_search
            .take()
            .unwrap_or((SearchOp::Move, 1, false));
        // Split the raw line into `{pattern}` + `{offset}` on the LAST unescaped delimiter (`/` forward,
        // `?` backward) — a `\/` in the pattern is literal (`:help search-offset`). `off` is `None` when
        // no offset part is present (`/foo`, `/foo/`), which distinguishes "reuse the last offset" from
        // "explicitly no offset" for the empty-pattern repeat below.
        let delim = if backward { '?' } else { '/' };
        let (raw_pattern, off) = split_search_offset(&line, delim);
        // Empty pattern → reuse the last search's PATTERN (Vim: `/<CR>` / `?<CR>` repeat it), adopting THIS
        // line's direction. A new offset on the empty line replaces the stored one; no offset part reuses
        // it (so `//e` re-searches the last pattern with `e`, and `//` repeats pattern AND offset).
        // With no previous pattern the search still runs, with an EMPTY pattern the core fails (E35, #1173).
        let (pattern, offset) = if raw_pattern.is_empty() {
            match &self.last_search {
                Some((p, _, last_off)) => (p.clone(), off.unwrap_or(*last_off)),
                None => (String::new(), SearchOffset::None),
            }
        } else {
            (raw_pattern, off.unwrap_or(SearchOffset::None))
        };
        if !pattern.is_empty() {
            self.last_search = Some((pattern.clone(), !backward, offset));
        }
        let cmd = Command::Search {
            op,
            count,
            pattern,
            backward,
            offset,
        };
        if filter {
            Feed::FilterSpan(cmd)
        } else {
            Feed::Cmd(cmd)
        }
    }

    /// `n` (`opposite = false`) / `N` (`opposite = true`): repeat the last search, REAPPLYING its offset.
    /// `n` keeps the stored direction, `N` flips it. Routed through [`Command::Search`] (carrying the
    /// offset) so the offset-aware match rule advances correctly even when the offset landed the cursor
    /// before the match. No prior search → a search with an EMPTY pattern, which the core fails and the
    /// frontend reports as E35: it reaches the core so the wanted column resets and a replay sees it, as nvim's
    /// `normal_search` sets `w_set_curswant` before it fails (#1173).
    pub(super) fn repeat_search(&mut self, opposite: bool) -> Feed {
        let (pattern, forward, offset) =
            self.last_search
                .clone()
                .unwrap_or((String::new(), true, SearchOffset::None));
        let backward = if opposite { forward } else { !forward };
        // `n`/`N` are motions: `3n` steps three matches and `dn`/`cn`/`gUn` operate up to the match
        // (#799), through the same operator and count model as `/`.
        self.span_action(Command::Search {
            op: self.search_op(),
            count: self.mcount(),
            pattern,
            backward,
            offset,
        })
    }

    /// Record `pattern` as the last search (with its `forward` direction) so `n`/`N` repeat it. Used by the
    /// frontend after it resolves a `*`/`#` (word-under-cursor) search, whose pattern is known only once the
    /// buffer is read: `*` is forward, `#` backward. These carry no offset ([`SearchOffset::None`]).
    pub fn set_last_search(&mut self, pattern: String, forward: bool) {
        self.last_search = Some((pattern, forward, SearchOffset::None));
    }

    /// Replace the last search PATTERN only, keeping its direction and offset (forward with no offset when
    /// there was none), as an Ex `:s/pat/` or `:g/pat/` does in Vim (`save_re_pat`): `n` then finds `pat`.
    pub fn set_last_search_pattern(&mut self, pattern: String) {
        let (forward, offset) = self
            .last_search
            .as_ref()
            .map_or((true, SearchOffset::None), |&(_, f, o)| (f, o));
        self.last_search = Some((pattern, forward, offset));
    }

    /// The whole last-search state (pattern, direction, offset), for `:keeppatterns` to put back (#609).
    pub(crate) fn search_state(&self) -> Option<(String, bool, SearchOffset)> {
        self.last_search.clone()
    }

    /// Put back a state [`InputEngine::search_state`] returned.
    pub(crate) fn restore_search_state(&mut self, state: Option<(String, bool, SearchOffset)>) {
        self.last_search = state;
    }

    /// The last search pattern (`/`, `?`, `*`, `#`), or `None` if nothing has been searched yet. The
    /// frontend reads it to resolve an empty `:s//repl/` pattern, which Vim fills from the last search.
    #[must_use]
    pub fn last_search(&self) -> Option<&str> {
        self.last_search.as_ref().map(|(p, _, _)| p.as_str())
    }
}

/// Split a raw search line into `(pattern, offset)` on the LAST unescaped `delim` (`/` for a forward
/// search, `?` for a backward one) — Vim's `/{pattern}/{offset}` grammar (`:help search-offset`). A
/// delimiter preceded by an odd run of backslashes is escaped (part of the pattern), so `foo\/bar/e`
/// splits into `("foo\/bar", Some(e))`. A present-but-unrecognized offset parses to [`SearchOffset::None`].
///
/// The returned offset is `None` ONLY when NO delimiter is present (`foo`); a delimiter with an empty
/// offset part (`foo/`, or the `//` repeat) yields `Some(SearchOffset::None)`. This distinction matters
/// for the empty-pattern repeat: a bare `/<CR>` (no delimiter) reuses the last offset, whereas `//<CR>`
/// (a typed delimiter, empty offset) explicitly resets to NO offset — matching nvim (`/foo/e` then `//`
/// lands on the match start, but then `/<CR>` reuses `e`).
/// Split a typed search line into `{pattern}` and `{offset}` at the delimiter that ends the pattern, found
/// as Vim's `skip_regexp` finds it: a `\/` is a literal, and so is a delimiter inside `[...]` (`/[/]`,
/// #803). `None` when there is no offset part.
pub(super) fn split_search_offset(line: &str, delim: char) -> (String, Option<SearchOffset>) {
    let idx = ruse_core::pattern::skip_regexp(line, delim, true);
    if idx < line.len() {
        (
            line[..idx].to_string(),
            Some(SearchOffset::parse(&line[idx + delim.len_utf8()..])),
        )
    } else {
        (line.to_string(), None)
    }
}
