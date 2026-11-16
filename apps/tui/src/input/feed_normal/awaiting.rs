//! The awaited second key (#1502): one decoder per `Awaiting` expectation, called from
//! `feed_awaiting` once the expectation is taken — the `f`/`t` target, the text-object selector, the
//! `g` second key (and its operator-pending check), the `r`/`gr` char, `CTRL-\`, `Z`, the mark names
//! after `` ` ``/`'`/`m`, the register name after `"` and the `[`/`]` commands.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ruse_core::{Command, Mode, Motion};

use super::super::{
    is_chord, keep_jumps_feed, mul_raw, plain, text_object, ExprTarget, Feed, InputEngine, Ns, Op,
    OpPending,
};

impl InputEngine {
    /// The target char after `f`/`F`/`t`/`T`.
    pub(super) fn await_find_target(&mut self, key: KeyEvent, forward: bool, till: bool) -> Feed {
        match key.code {
            KeyCode::Char(ch) => {
                self.last_find = Some((ch, forward, till));
                self.motion(Motion::FindChar {
                    ch,
                    forward,
                    till,
                    repeat: false,
                })
            }
            // A pending construct is in flight, so this is `closed/abort` — the policy
            // that distinguishes operator-pending from Normal (VS-OBL-3).
            _ => self.unmatched(Ns::OperatorPending, key),
        }
    }

    /// The text-object selector after `i`/`a` (under an operator or in a selection).
    pub(super) fn await_text_object(&mut self, key: KeyEvent, inner: bool) -> Feed {
        match key.code {
            // Under an operator this composes (`diw`/`da(`/`ci"`); in a selection `self.motion`
            // emits a bare `Move` whose text-object shape the core turns into a selection (`viw`).
            KeyCode::Char(ch) if text_object(ch, inner).is_some() => {
                self.motion(text_object(ch, inner).expect("guarded by is_some"))
            }
            // Not a text-object selector (`t` IS one — `it`/`at` tag objects resolve via
            // `text_object` → `Motion::Tag` above): a pending construct is in flight, so this is
            // `closed/abort` — the operator-pending policy (VS-OBL-3).
            _ => self.unmatched(Ns::OperatorPending, key),
        }
    }

    /// The second key of a `g` command.
    pub(super) fn await_g_second(&mut self, key: KeyEvent, mode: Mode) -> Feed {
        // `g<Down>`/`g<Up>`/`g<Home>`/`g<End>` are `gj`/`gk`/`g0`/`g$` (`:help g<Down>`).
        let key = match key.code {
            KeyCode::Down if key.modifiers.is_empty() => plain(KeyCode::Char('j')),
            KeyCode::Up if key.modifiers.is_empty() => plain(KeyCode::Char('k')),
            KeyCode::Home if key.modifiers.is_empty() => plain(KeyCode::Char('0')),
            KeyCode::End if key.modifiers.is_empty() => plain(KeyCode::Char('$')),
            _ => key,
        };
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        // `g CTRL-G` and Visual `g CTRL-A`/`g CTRL-X` are the only `g` chords; any other one aborts
        // instead of reaching the bare `g{char}` arms (`g<C-e>` is not `ge`).
        let g_chord = match key.code {
            KeyCode::Char('g') => true,
            KeyCode::Char('a' | 'x') => matches!(mode, Mode::Visual { .. }),
            KeyCode::Char('h') => mode == Mode::Normal,
            _ => false,
        };
        if is_chord(key) && matches!(key.code, KeyCode::Char(_)) && !(ctrl && g_chord) {
            return self.unmatched(Ns::OperatorPending, key);
        }
        // Under an operator only the `g` MOTIONS continue it, and the doubled form of the `g`
        // operators (`gugu`, `gUgU`, `g~g~`, `g?g?`, `gqgq`, `gwgw`) is its linewise form; any other
        // `g` command (`dgJ`, `dgp`, `ygu`, ...) aborts instead of running bare (#790).
        if let Some(p) = self.normal.op {
            if let Some(feed) = self.g_under_op(p, key, ctrl) {
                return feed;
            }
        }
        self.g_command(key, mode, ctrl)
    }

    /// The `g` second key under an operator: `None` when it continues the operator as a `g` motion.
    fn g_under_op(&mut self, p: OpPending, key: KeyEvent, ctrl: bool) -> Option<Feed> {
        let KeyCode::Char(c) = key.code else {
            return Some(self.unmatched(Ns::OperatorPending, key));
        };
        let doubled = matches!(
            (p.op, c),
            (Op::CaseLower, 'u')
                | (Op::CaseUpper, 'U')
                | (Op::CaseToggle, '~')
                | (Op::Rot13, '?')
                | (Op::Format, 'q')
                | (Op::FormatKeep, 'w')
        );
        if !ctrl && doubled {
            return Some(self.op_linewise());
        }
        let g_motion = matches!(
            c,
            'g' | 'e'
                | 'E'
                | '_'
                | 'j'
                | 'k'
                | '0'
                | '$'
                | '^'
                | 'm'
                | 'o'
                | 'M'
                | 'n'
                | 'N'
                | '*'
                | '#'
                | '`'
                | '\''
        );
        if ctrl || !g_motion {
            return Some(self.unmatched(Ns::OperatorPending, key));
        }
        None
    }

    /// The char after `r` (Normal) or `v_r` (a selection).
    pub(super) fn await_replace_char(&mut self, key: KeyEvent, mode: Mode) -> Feed {
        match key.code {
            // In a selection, `r{char}` replaces EVERY selected char with it (Vim `v_r`); in Normal
            // it replaces `count` chars under the cursor. The count accumulated before `r` is still
            // live (the `r` arm did not reset it).
            KeyCode::Char(c) if matches!(mode, Mode::Visual { .. } | Mode::Select { .. }) => {
                self.action(Command::ReplaceSelectionChar(c))
            }
            KeyCode::Char(c) => self.action(Command::ReplaceChar(self.mcount(), c)),
            // `r<CR>` replaces the char(s) with a line break (Vim splits the line). Enter is a
            // distinct KeyCode, so map it to a `\n` replacement here.
            KeyCode::Enter if matches!(mode, Mode::Visual { .. } | Mode::Select { .. }) => {
                self.action(Command::ReplaceSelectionChar('\n'))
            }
            KeyCode::Enter => self.action(Command::ReplaceChar(self.mcount(), '\n')),
            // A pending construct is in flight, so this is `closed/abort` — the policy
            // that distinguishes operator-pending from Normal (VS-OBL-3).
            _ => self.unmatched(Ns::OperatorPending, key),
        }
    }

    /// The char after `gr`.
    pub(super) fn await_virtual_replace_char(&mut self, key: KeyEvent) -> Feed {
        match key.code {
            // `{count}gr{char}` — one-shot virtual replace (tab-aware) of `count` chars, back to
            // Normal. The count accumulated before `gr` is still live (the `gr` arm did not reset it).
            // No `<CR>` form: classic `gr<CR>` is not the line-splitting `r<CR>` — the char is taken
            // literally, so a bare Enter here simply aborts the pending construct.
            KeyCode::Char(c) => self.action(Command::VirtualReplaceChar(self.mcount(), c)),
            _ => self.unmatched(Ns::OperatorPending, key),
        }
    }

    /// The key after `CTRL-\`.
    pub(super) fn await_ctrl_backslash(&mut self, key: KeyEvent, mode: Mode) -> Feed {
        let only_ctrl = key.modifiers - KeyModifiers::SHIFT == KeyModifiers::CONTROL;
        match key.code {
            KeyCode::Char('n' | 'g') if only_ctrl => {
                if mode == Mode::Normal {
                    self.reset();
                    Feed::Ignored
                } else {
                    self.action(Command::EnterNormal)
                }
            }
            _ => self.unmatched(Ns::OperatorPending, key),
        }
    }

    /// The key after `Z` (`ZZ` / `ZQ`).
    pub(super) fn await_z_second(&mut self, key: KeyEvent) -> Feed {
        let line = match key.code {
            KeyCode::Char('Z') if key.modifiers - KeyModifiers::SHIFT == KeyModifiers::NONE => "x",
            KeyCode::Char('Q') if key.modifiers - KeyModifiers::SHIFT == KeyModifiers::NONE => "q!",
            _ => return self.unmatched(Ns::OperatorPending, key),
        };
        self.reset();
        Feed::ExecuteEx(line.to_string())
    }

    /// Whether a mark name after `` ` `` / `'` aborts the armed operator. Only a named mark composes
    /// with an operator here (`` d`a ``); the special marks do not yet, so under an operator they abort
    /// rather than jump with the operator dropped (#790).
    fn mark_aborts_op(&self, key: KeyEvent) -> bool {
        self.normal.op.is_some()
            && !matches!(
                key.code,
                KeyCode::Char('a'..='z' | 'A'..='Z' | '0'..='9' | '^' | '<' | '>')
            )
    }

    /// The mark name after `` ` `` (or `` g` ``): a charwise mark jump.
    pub(super) fn await_mark_jump(&mut self, key: KeyEvent, keep_jumps: bool) -> Feed {
        let jump = self.normal.op.is_none();
        if self.mark_aborts_op(key) {
            return self.unmatched(Ns::OperatorPending, key);
        }
        let feed = match key.code {
            // `` `^ `` (where Insert stopped), and `` d`< ``/`` d`> `` under an operator (#1088).
            KeyCode::Char('^') => self.mark_op('^', false),
            KeyCode::Char(c @ ('<' | '>')) if self.normal.op.is_some() => self.mark_op(c, false),
            // `` `. `` — jump to the last-change mark.
            KeyCode::Char('.') => self.action(Command::GotoLastChange),
            // `` `` `` / `` `' `` — jump to the context mark (position before the latest jump).
            KeyCode::Char('`') | KeyCode::Char('\'') => self.action(Command::GotoContextMark),
            // `` `[ `` / `` `] `` — jump charwise to the first/last char of the last changed/yanked text.
            KeyCode::Char('[') => self.action(Command::GotoChangeMarkStart),
            KeyCode::Char(']') => self.action(Command::GotoChangeMarkEnd),
            // `` `< `` / `` `> `` — jump charwise to the start/end of the last visual selection.
            KeyCode::Char('<') => self.action(Command::GotoVisualMarkStart),
            KeyCode::Char('>') => self.action(Command::GotoVisualMarkEnd),
            // `` `{a-z} `` — jump to a named mark, or `` d`a ``/`` y`a `` with an operator pending; a
            // numbered one (`` `1 ``, set by `:mark 1`, #1241) the same way.
            KeyCode::Char(c @ ('a'..='z' | 'A'..='Z' | '0'..='9')) => self.mark_op(c, false),
            // Any other mark name is not wired — abort the pending construct.
            _ => self.unmatched(Ns::OperatorPending, key),
        };
        keep_jumps_feed(feed, keep_jumps && jump)
    }

    /// The mark name after `m`.
    pub(super) fn await_set_mark(&mut self, key: KeyEvent) -> Feed {
        match key.code {
            // `m{a-zA-Z}` — set a named mark at the cursor (an uppercase one stays with this
            // buffer, #1088).
            KeyCode::Char(c @ ('a'..='z' | 'A'..='Z')) => self.action(Command::SetNamedMark(c)),
            // The other marks `m` can set are deferred — abort.
            _ => self.unmatched(Ns::OperatorPending, key),
        }
    }

    /// The mark name after `'` (or `g'`): a linewise mark jump.
    pub(super) fn await_mark_jump_line(&mut self, key: KeyEvent, keep_jumps: bool) -> Feed {
        let jump = self.normal.op.is_none();
        if self.mark_aborts_op(key) {
            return self.unmatched(Ns::OperatorPending, key);
        }
        let feed = match key.code {
            // `'^`, and `d'<`/`d'>` under an operator (#1088).
            KeyCode::Char('^') => self.mark_op('^', true),
            KeyCode::Char(c @ ('<' | '>')) if self.normal.op.is_some() => self.mark_op(c, true),
            // `'.` — linewise to the last-change line.
            KeyCode::Char('.') => self.action(Command::GotoLastChangeLine),
            // `''` / `` '` `` — linewise to the context mark's line (position before the latest jump).
            KeyCode::Char('\'') | KeyCode::Char('`') => self.action(Command::GotoContextMarkLine),
            // `'[` / `']` — linewise to the first non-blank of the first/last changed/yanked line.
            KeyCode::Char('[') => self.action(Command::GotoChangeMarkStartLine),
            KeyCode::Char(']') => self.action(Command::GotoChangeMarkEndLine),
            // `'<` / `'>` — linewise to the first non-blank of the first/last selected line.
            KeyCode::Char('<') => self.action(Command::GotoVisualMarkStartLine),
            KeyCode::Char('>') => self.action(Command::GotoVisualMarkEndLine),
            // `'{a-z}` — linewise to a named mark's line, or `d'a`/`y'a` with an operator pending; a
            // numbered one (`'1`, #1241) the same way.
            KeyCode::Char(c @ ('a'..='z' | 'A'..='Z' | '0'..='9')) => self.mark_op(c, true),
            // Any other mark name is not wired — abort.
            _ => self.unmatched(Ns::OperatorPending, key),
        };
        keep_jumps_feed(feed, keep_jumps && jump)
    }

    /// The register name after `"`.
    pub(super) fn await_register(&mut self, key: KeyEvent) -> Feed {
        // `"=` — the expression register (`:help quote=`): open the expression prompt instead of
        // arming a stored slot. On `<CR>` the collected expression is evaluated and the `"=` register
        // armed, so the FOLLOWING `p`/`P` pastes the result. Reset first so no partial Normal state
        // (a stray count/operator) survives into the prompt.
        if key.code == KeyCode::Char('=') {
            self.reset();
            self.open_expr_prompt(ExprTarget::Paste);
            return Feed::Pending;
        }
        match key.code {
            // A register name — a letter (`a`–`z` / `A`–`Z`), the yank register `0`, the numbered
            // delete-ring `1`–`9`, the small-delete `-`, or the blackhole `_`: emit `SetRegister` for the core
            // to hold as the pending register the next yank/delete/change/paste reads. A PREFIX, not
            // a command: the count typed so far survives as its own segment (`3"ayy` yanks 3,
            // `2"a3x` cuts 6) and an `i_CTRL-O` one-shot stays in flight for the command that follows
            // (`i<C-o>"ap` pastes) — #796. `"0p`/`"1p`/`"-p` paste from read-only edit-history slots.
            KeyCode::Char(c)
                if c.is_ascii_alphabetic()
                    || c.is_ascii_digit()
                    || c == '-'
                    || c == '_'
                    // `"+`/`"*` — the system clipboard (`:help quoteplus`).
                    || c == '+'
                    || c == '*'
                    // The read-only special registers `"/ ": ". "%` as a paste SOURCE (`"/p`,
                    // `".p`, `"%p`): arm them like any slot; the following `p`/`P` reads the
                    // frontend-synced value. A yank/delete naming one fails (read-only, #1022).
                    || c == '/'
                    || c == ':'
                    || c == '.'
                    || c == '%'
                    // `"#` — the alternate file name (`:help quote#`).
                    || c == '#' =>
            {
                self.normal.reg_count = mul_raw(self.normal.reg_count, self.normal.count);
                self.normal.count = 0;
                self.normal.register = true;
                Feed::Cmd(Command::SetRegister(Some(c)))
            }
            // Any other name is unusable; a pending construct is in flight, so it is `closed/abort`
            // (operator-pending), leaking no state.
            _ => self.unmatched(Ns::OperatorPending, key),
        }
    }

    /// The key after `[` / `]`.
    pub(super) fn await_bracket(&mut self, key: KeyEvent, open_bracket: bool) -> Feed {
        // `]p`/`[i`/... are commands, not motions: under an operator they abort (`d]p`, #790).
        if self.normal.op.is_some() && matches!(key.code, KeyCode::Char('p' | 'P' | 'i' | 'I')) {
            return self.unmatched(Ns::OperatorPending, key);
        }
        let count = self.mcount();
        match key.code {
            // Indent-adjusting paste. Only `]p` pastes AFTER (below); `]P`, `[p`, `[P` all paste
            // BEFORE (above), matching Vim. Other bracket commands (`[(`, `]}`, …) are not wired.
            KeyCode::Char('p') => self.action(Command::PasteIndent {
                after: !open_bracket,
                count,
            }),
            KeyCode::Char('P') => self.action(Command::PasteIndent {
                after: false,
                count,
            }),
            // Section motions — count/operator-aware via `self.motion` (which reads the preserved
            // count and composes any armed `d`/`c`/`y`). `]]`/`[[` seek a `{` (or form-feed) in
            // column 0; `][`/`[]` seek a `}` (or form-feed). The starting bracket picks the axis.
            KeyCode::Char(']') if !open_bracket => self.motion(Motion::SectionFwd),
            KeyCode::Char('[') if !open_bracket => self.motion(Motion::SectionEndFwd),
            KeyCode::Char('[') if open_bracket => self.motion(Motion::SectionBack),
            KeyCode::Char(']') if open_bracket => self.motion(Motion::SectionEndBack),
            // Unmatched-paren/brace motions — count/operator-aware via `self.motion`. `[(`/`[{` go to
            // the previous unmatched `(`/`{`; `])`/`]}` go to the next unmatched `)`/`}`. The starting
            // bracket (`[` vs `]`) fixes the direction; the pressed char picks paren vs brace.
            KeyCode::Char('(') if open_bracket => self.motion(Motion::UnmatchedParenBack),
            KeyCode::Char('{') if open_bracket => self.motion(Motion::UnmatchedBraceBack),
            KeyCode::Char(')') if !open_bracket => self.motion(Motion::UnmatchedParenFwd),
            KeyCode::Char('}') if !open_bracket => self.motion(Motion::UnmatchedBraceFwd),
            // Method (brace-block) motions — count/operator-aware via `self.motion`. `]m`/`[m` go to
            // the next/previous method START (a `{`); `]M`/`[M` go to the next/previous method END (a
            // `}`). The starting bracket (`]` vs `[`) fixes the direction; the case picks start vs end.
            KeyCode::Char('m') if !open_bracket => self.motion(Motion::MethodStartFwd),
            KeyCode::Char('m') if open_bracket => self.motion(Motion::MethodStartBack),
            KeyCode::Char('M') if !open_bracket => self.motion(Motion::MethodEndFwd),
            KeyCode::Char('M') if open_bracket => self.motion(Motion::MethodEndBack),
            // Keyword-line lookup (`:help [i`) — DISPLAY the line(s) containing the keyword under
            // the cursor (current buffer). `[i`/`]i` echo the count'th match (`[` from the top of
            // the file, `]` below the cursor); `[I`/`]I` list all matches. The frontend resolves
            // the keyword and scans the lines (the engine has no buffer), like `*`/`gd`.
            KeyCode::Char('i') => self.action(Command::ShowKeywordLines {
                above: open_bracket,
                list: false,
                count,
            }),
            KeyCode::Char('I') => self.action(Command::ShowKeywordLines {
                above: open_bracket,
                list: true,
                count,
            }),
            _ => self.unmatched(Ns::OperatorPending, key),
        }
    }
}
