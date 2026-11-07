//! Running a parsed `:set` / `:setlocal` / `:setglobal` line (#614) against the option values the
//! session and the core hold, and the provenance each value carries (`:verbose set opt?`).
//!
//! The grammar and the option table are `input/set_cmd.rs`; this module owns where each value lives:
//! the core's View options (`ic`, `scs`, `ts`, `sw`, `et`, `tw`, `sts`, `ai`, `si`, `ci`, `pi`, `sta`, `sr`,
//! local per View plus the Workspace's global copy), the session's render and write preferences (`hls`, `is`, `fixeol`, `so`), the input engine's
//! `'tildeop'`, and the per-buffer fold state (`fen`, `fdm`, `fdl`).

use std::collections::HashMap;

use ruse_core::editor::EditorOption;
use ruse_core::{Command, DocumentId, ExCmd, OptionLevel, Revision, Workspace};

use crate::input::set_cmd::{
    assign_value, listing, opt_def, show_one, Locality, Opt, OptDef, OptKind, OptVal, SetArg,
    SetLine, SetScope, OPTIONS,
};
use crate::input::{FoldMethod, InputEngine};
use crate::ui::window_view::DisplayGlobals;
use ruse_core::WindowOptions;

/// Where an option's value came from (CFG-PROV-001): what `:verbose set opt?` reports. An option with no
/// entry still has its default.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) enum OptSource {
    /// Typed (`:set`): nvim prints no `Last set from` line for it, and a config reload leaves it alone.
    Typed,
    /// A config file: its layer, the file and the 1-based line.
    Config {
        layer: &'static str,
        file: String,
        line: usize,
    },
}

/// The option values a `:set` line reads and writes, borrowed from the session for one command.
pub(crate) struct OptionHost<'a> {
    pub ws: &'a mut Workspace,
    pub recorded: &'a mut Vec<Command>,
    pub engine: &'a mut InputEngine,
    pub hlsearch_on: &'a mut bool,
    pub search_hl: &'a mut Option<String>,
    pub incsearch_on: &'a mut bool,
    pub scrolloff: &'a mut i64,
    pub folds: &'a mut HashMap<DocumentId, Vec<crate::folds::Fold>>,
    pub fold_method: &'a mut HashMap<DocumentId, FoldMethod>,
    pub foldlevel: &'a mut HashMap<DocumentId, usize>,
    pub foldenable: &'a mut HashMap<DocumentId, bool>,
    pub fold_rev: &'a mut HashMap<DocumentId, Revision>,
    pub sources: &'a mut HashMap<Opt, OptSource>,
    pub ui: &'a mut UiOptions,
    /// The buffers' files and `'readonly'` (#1211).
    pub files: &'a mut crate::app::dispatch::Files,
    pub readonly: &'a mut crate::app::dispatch::ReadOnly,
    /// The focused window's text rows: `'scroll'` reads and checks against it (0 when unknown).
    pub win_height: usize,
}

/// The session's own option values that no other owner holds (#1151).
#[derive(Clone, Debug)]
pub(crate) struct UiOptions {
    /// `'showmode'`: the status line names the mode.
    pub showmode: bool,
    /// `'verbose'`: above 0, `:set opt?` adds the `Last set from` line.
    pub verbose: i64,
    /// The global copies of the global and global-local display options (#615, #616).
    pub display: DisplayGlobals,
    /// `'messagesopt'` as set; its `history:` item sizes the `:messages` history (#1235).
    pub messagesopt: String,
    /// `'foldopen'` as set (#1141).
    pub foldopen: String,
    /// `render.conceal` (F-031): marked-up byte ranges are hidden off the caret line. A config key with
    /// no `:set` option (nvim's `'conceallevel'` is per-syntax-group, which ruse does not model).
    pub conceal: bool,
    /// `'report'`: the line count a command's report needs to pass (#1276).
    pub report: i64,
    /// `'hidden'`, `'confirm'`, `'autowrite'`, `'autowriteall'`, `'autoread'` (#619).
    pub buffers: crate::app::abandon::BufferOptions,
}

impl UiOptions {
    /// The `:messages` history size `'messagesopt'` gives (its value was checked when it was set).
    pub(crate) fn message_history(&self) -> usize {
        crate::input::set_cmd::messagesopt_history(&self.messagesopt)
            .unwrap_or(crate::app::messages::HISTORY_DEFAULT)
    }
}

impl Default for UiOptions {
    fn default() -> UiOptions {
        UiOptions {
            showmode: true,
            verbose: 0,
            display: DisplayGlobals::default(),
            messagesopt: crate::input::set_cmd::MESSAGESOPT.to_string(),
            foldopen: crate::input::set_cmd::FOLDOPEN.to_string(),
            conceal: true,
            report: crate::app::report::REPORT_DEFAULT,
            buffers: crate::app::abandon::BufferOptions::default(),
        }
    }
}

/// A window-local display option's value in `o` (#615, #616), `None` for any other option.
fn window_value(o: &WindowOptions, opt: Opt) -> Option<OptVal> {
    Some(match opt {
        Opt::Wrap => OptVal::Bool(o.wrap),
        Opt::LineBreak => OptVal::Bool(o.linebreak),
        Opt::BreakIndent => OptVal::Bool(o.breakindent),
        Opt::BreakIndentOptions => OptVal::Text(o.breakindentopt.clone().into()),
        Opt::Number => OptVal::Bool(o.number),
        Opt::RelativeNumber => OptVal::Bool(o.relativenumber),
        Opt::NumberWidth => OptVal::Num(o.numberwidth),
        Opt::SignColumn => OptVal::Text(o.signcolumn.clone().into()),
        Opt::List => OptVal::Bool(o.list),
        Opt::CursorLine => OptVal::Bool(o.cursorline),
        Opt::CursorLineOptions => OptVal::Text(o.cursorlineopt.clone().into()),
        Opt::CursorColumn => OptVal::Bool(o.cursorcolumn),
        Opt::ColorColumn => OptVal::Text(o.colorcolumn.clone().into()),
        Opt::FoldColumn => OptVal::Text(o.foldcolumn.clone().into()),
        Opt::SmoothScroll => OptVal::Bool(o.smoothscroll),
        _ => return None,
    })
}

/// Write a window-local display option's value into `o`.
fn set_window_value(o: &mut WindowOptions, opt: Opt, val: &OptVal) {
    let on = matches!(val, OptVal::Bool(true));
    let text = || match val {
        OptVal::Text(t) => t.to_string(),
        _ => String::new(),
    };
    match (opt, val) {
        (Opt::Wrap, _) => o.wrap = on,
        (Opt::LineBreak, _) => o.linebreak = on,
        (Opt::BreakIndent, _) => o.breakindent = on,
        (Opt::BreakIndentOptions, _) => o.breakindentopt = text(),
        (Opt::Number, _) => o.number = on,
        (Opt::RelativeNumber, _) => o.relativenumber = on,
        (Opt::NumberWidth, OptVal::Num(n)) => o.numberwidth = *n,
        (Opt::SignColumn, _) => o.signcolumn = text(),
        (Opt::List, _) => o.list = on,
        (Opt::CursorLine, _) => o.cursorline = on,
        (Opt::CursorLineOptions, _) => o.cursorlineopt = text(),
        (Opt::CursorColumn, _) => o.cursorcolumn = on,
        (Opt::ColorColumn, _) => o.colorcolumn = text(),
        (Opt::FoldColumn, _) => o.foldcolumn = text(),
        (Opt::SmoothScroll, _) => o.smoothscroll = on,
        _ => {}
    }
}

/// What a `:set` line printed: the rows (`:set ts?`, listings) and the error that stopped it, if any.
#[derive(Default, Debug)]
pub(crate) struct SetOutcome {
    pub rows: Vec<String>,
    pub error: Option<String>,
}

fn core_option(opt: Opt, val: &OptVal) -> Option<EditorOption> {
    let n = |v: &OptVal| match v {
        OptVal::Num(n) => usize::try_from(*n).unwrap_or(0),
        _ => 0,
    };
    let b = |v: &OptVal| matches!(v, OptVal::Bool(true));
    let s = |v: &OptVal| match v {
        OptVal::Text(t) => t.to_string(),
        _ => String::new(),
    };
    Some(match opt {
        Opt::FormatOptions => EditorOption::FormatOptions(s(val)),
        Opt::Comments => EditorOption::Comments(s(val)),
        Opt::CommentString => EditorOption::CommentString(s(val)),
        Opt::FormatListPat => EditorOption::FormatListPat(s(val)),
        Opt::FormatPrg => EditorOption::FormatPrg(s(val)),
        Opt::EqualPrg => EditorOption::EqualPrg(s(val)),
        Opt::CinWords => EditorOption::CinWords(s(val)),
        Opt::LispWords => EditorOption::LispWords(s(val)),
        Opt::Lisp => EditorOption::Lisp(b(val)),
        Opt::CIndent => EditorOption::CIndent(b(val)),
        Opt::VarTabStop => EditorOption::VarTabStop(s(val)),
        Opt::VarSoftTabStop => EditorOption::VarSoftTabStop(s(val)),
        Opt::CinOptions => EditorOption::CinOptions(s(val)),
        Opt::CinKeys => EditorOption::CinKeys(s(val)),
        Opt::CinScopeDecls => EditorOption::CinScopeDecls(s(val)),
        Opt::JoinSpaces => EditorOption::JoinSpaces(b(val)),
        Opt::WrapMargin => EditorOption::WrapMargin(match val {
            OptVal::Num(n) => *n,
            _ => 0,
        }),
        Opt::IgnoreCase => EditorOption::IgnoreCase(b(val)),
        Opt::SmartCase => EditorOption::SmartCase(b(val)),
        Opt::TabStop => EditorOption::TabStop(n(val)),
        Opt::ShiftWidth => EditorOption::ShiftWidth(n(val)),
        Opt::ExpandTab => EditorOption::ExpandTab(b(val)),
        Opt::TextWidth => EditorOption::TextWidth(n(val)),
        Opt::SoftTabStop => EditorOption::SoftTabStop(match val {
            OptVal::Num(n) => *n,
            _ => 0,
        }),
        Opt::AutoIndent => EditorOption::AutoIndent(b(val)),
        Opt::SmartIndent => EditorOption::SmartIndent(b(val)),
        Opt::CopyIndent => EditorOption::CopyIndent(b(val)),
        Opt::PreserveIndent => EditorOption::PreserveIndent(b(val)),
        Opt::SmartTab => EditorOption::SmartTab(b(val)),
        Opt::ShiftRound => EditorOption::ShiftRound(b(val)),
        _ => return None,
    })
}

fn core_value(opts: &[EditorOption], opt: Opt) -> Option<OptVal> {
    opts.iter().find_map(|o| match (opt, o.clone()) {
        (Opt::FormatOptions, EditorOption::FormatOptions(s))
        | (Opt::Comments, EditorOption::Comments(s))
        | (Opt::CommentString, EditorOption::CommentString(s))
        | (Opt::FormatListPat, EditorOption::FormatListPat(s))
        | (Opt::FormatPrg, EditorOption::FormatPrg(s))
        | (Opt::EqualPrg, EditorOption::EqualPrg(s))
        | (Opt::CinWords, EditorOption::CinWords(s))
        | (Opt::LispWords, EditorOption::LispWords(s))
        | (Opt::CinOptions, EditorOption::CinOptions(s))
        | (Opt::CinKeys, EditorOption::CinKeys(s))
        | (Opt::VarTabStop, EditorOption::VarTabStop(s))
        | (Opt::VarSoftTabStop, EditorOption::VarSoftTabStop(s))
        | (Opt::CinScopeDecls, EditorOption::CinScopeDecls(s)) => Some(OptVal::Text(s.into())),
        (Opt::WrapMargin, EditorOption::WrapMargin(n)) => Some(OptVal::Num(n)),
        (Opt::JoinSpaces, EditorOption::JoinSpaces(v))
        | (Opt::Lisp, EditorOption::Lisp(v))
        | (Opt::CIndent, EditorOption::CIndent(v))
        | (Opt::IgnoreCase, EditorOption::IgnoreCase(v))
        | (Opt::SmartCase, EditorOption::SmartCase(v))
        | (Opt::ExpandTab, EditorOption::ExpandTab(v))
        | (Opt::AutoIndent, EditorOption::AutoIndent(v))
        | (Opt::SmartIndent, EditorOption::SmartIndent(v))
        | (Opt::CopyIndent, EditorOption::CopyIndent(v))
        | (Opt::PreserveIndent, EditorOption::PreserveIndent(v))
        | (Opt::SmartTab, EditorOption::SmartTab(v))
        | (Opt::ShiftRound, EditorOption::ShiftRound(v)) => Some(OptVal::Bool(v)),
        (Opt::SoftTabStop, EditorOption::SoftTabStop(n)) => Some(OptVal::Num(n)),
        (Opt::TabStop, EditorOption::TabStop(n))
        | (Opt::ShiftWidth, EditorOption::ShiftWidth(n))
        | (Opt::TextWidth, EditorOption::TextWidth(n)) => {
            Some(OptVal::Num(i64::try_from(n).unwrap_or(i64::MAX)))
        }
        _ => None,
    })
}

impl OptionHost<'_> {
    /// The value of `opt`: the global copy when `global`, else the focused window's.
    pub(crate) fn get(&self, opt: Opt, global: bool) -> OptVal {
        let opts = if global {
            self.ws.global_editing_options()
        } else {
            self.ws.focused().view.editing_options()
        };
        if let Some(v) = core_value(&opts, opt) {
            return v;
        }
        let win = if global {
            self.ws.window_defaults()
        } else {
            self.ws.focused().view.window_options()
        };
        if let Some(v) = window_value(win, opt) {
            return v;
        }
        let d = &self.ui.display;
        let local = (!global).then(|| self.ws.focused().view.window_options());
        let buf = self.ws.focused_buffer();
        match opt {
            // Global-local: the window's own value when it has one, else the global one.
            Opt::ShowBreak => OptVal::Text(
                local
                    .and_then(|o| o.showbreak.clone())
                    .unwrap_or_else(|| d.showbreak.clone())
                    .into(),
            ),
            Opt::ListChars => OptVal::Text(
                local
                    .and_then(|o| o.listchars.clone())
                    .unwrap_or_else(|| d.listchars.clone())
                    .into(),
            ),
            Opt::FillChars => OptVal::Text(
                local
                    .and_then(|o| o.fillchars.clone())
                    .unwrap_or_else(|| d.fillchars.clone())
                    .into(),
            ),
            // 'scroll': the window's value at its height (half the height until a count or `:set` gives
            // it one).
            Opt::Scroll => OptVal::Num(
                i64::try_from(self.ws.focused().view.scroll(self.win_height)).unwrap_or(0),
            ),
            Opt::SideScrollOff => OptVal::Num(
                local
                    .and_then(|o| o.sidescrolloff)
                    .unwrap_or(d.sidescrolloff),
            ),
            Opt::BreakAt => OptVal::Text(d.breakat.clone().into()),
            Opt::Display => OptVal::Text(d.display.clone().into()),
            Opt::SideScroll => OptVal::Num(d.sidescroll),
            Opt::ScrollJump => OptVal::Num(d.scrolljump),
            Opt::HlSearch => OptVal::Bool(*self.hlsearch_on),
            Opt::IncSearch => OptVal::Bool(*self.incsearch_on),
            Opt::FixEol => OptVal::Bool(self.files.write.fixeol),
            Opt::TildeOp => OptVal::Bool(self.engine.tildeop()),
            // The window's own value, else the global one (`:set so?` shows the value in force).
            Opt::ScrollOff if global => OptVal::Num(*self.scrolloff),
            Opt::ScrollOff => OptVal::Num(
                self.ws
                    .focused()
                    .view
                    .scrolloff_local()
                    .unwrap_or(*self.scrolloff),
            ),
            Opt::ShowMode => OptVal::Bool(self.ui.showmode),
            Opt::Verbose => OptVal::Num(self.ui.verbose),
            Opt::MessageOptions => OptVal::Text(self.ui.messagesopt.clone().into()),
            Opt::FoldOpen => OptVal::Text(self.ui.foldopen.clone().into()),
            Opt::Report => OptVal::Num(self.ui.report),
            Opt::History => {
                OptVal::Num(i64::try_from(self.engine.history_len()).unwrap_or(i64::MAX))
            }
            Opt::CpOptions => OptVal::Text(crate::input::set_cmd::CPO_DEFAULT.into()),
            Opt::VerboseFile => OptVal::Text("".into()),
            Opt::FoldEnable => OptVal::Bool(self.foldenable.get(&buf).copied().unwrap_or(true)),
            Opt::ReadOnly => OptVal::Bool(self.readonly.get(self.files, buf)),
            Opt::Backup => OptVal::Bool(self.files.write.backup),
            Opt::WriteBackup => OptVal::Bool(self.files.write.writebackup),
            Opt::Write => OptVal::Bool(self.files.write.write),
            Opt::WriteAny => OptVal::Bool(self.files.write.writeany),
            Opt::Fsync => OptVal::Bool(self.files.write.fsync),
            Opt::BackupDir => OptVal::Text(self.files.write.backupdir().into()),
            Opt::BackupSkip => OptVal::Text(self.files.write.backupskip().into()),
            Opt::BackupExt => OptVal::Text(self.files.write.backupext.clone().into()),
            Opt::BackupCopy => OptVal::Text(self.files.write.backupcopy.clone().into()),
            Opt::PatchMode => OptVal::Text(self.files.write.patchmode.clone().into()),
            Opt::EndOfLine => OptVal::Bool(self.file_format(buf).final_newline),
            Opt::FileFormat => OptVal::Str(self.file_format(buf).eol.name()),
            Opt::Binary => OptVal::Bool(self.file_format(buf).binary),
            Opt::FileFormats => OptVal::Text(self.files.write.fileformats.clone().into()),
            Opt::AutoRead => OptVal::Bool(self.ui.buffers.autoread),
            Opt::AutoWrite => OptVal::Bool(self.ui.buffers.autowrite),
            Opt::AutoWriteAll => OptVal::Bool(self.ui.buffers.autowriteall),
            Opt::Confirm => OptVal::Bool(self.ui.buffers.confirm),
            Opt::Hidden => OptVal::Bool(self.ui.buffers.hidden),
            Opt::Modifiable => OptVal::Bool(
                self.ws
                    .document(buf)
                    .is_none_or(ruse_core::Document::is_modifiable),
            ),
            Opt::Modified => OptVal::Bool(self.ws.document(buf).is_some_and(|d| d.is_modified())),
            Opt::EndOfFile => OptVal::Bool(self.file_format(buf).eof),
            Opt::Bomb => OptVal::Bool(self.file_format(buf).bom),
            Opt::FoldLevel => OptVal::Num(
                i64::try_from(self.foldlevel.get(&buf).copied().unwrap_or(0)).unwrap_or(i64::MAX),
            ),
            Opt::FoldMethod => OptVal::Str(match self.fold_method.get(&buf) {
                Some(FoldMethod::Indent) => "indent",
                _ => "manual",
            }),
            _ => unreachable!("core options are read above"),
        }
    }

    /// The value `scope` shows: as [`OptionHost::get`], except that `:setlocal` on a global-local
    /// option shows the window's own value, `-1` when it has none (nvim).
    pub(crate) fn read(&self, opt: Opt, scope: SetScope) -> OptVal {
        if scope == SetScope::Local && opt_def(opt).locality == Locality::GlobalLocal {
            let o = self.ws.focused().view.window_options();
            return match opt {
                Opt::ShowBreak => OptVal::Text(o.showbreak.clone().unwrap_or_default().into()),
                Opt::ListChars => OptVal::Text(o.listchars.clone().unwrap_or_default().into()),
                Opt::FillChars => OptVal::Text(o.fillchars.clone().unwrap_or_default().into()),
                Opt::SideScrollOff => OptVal::Num(o.sidescrolloff.unwrap_or(-1)),
                _ => OptVal::Num(self.ws.focused().view.scrolloff_local().unwrap_or(-1)),
            };
        }
        self.get(opt, scope == SetScope::Global)
    }

    /// Write `val` into `opt` at `level`.
    pub(crate) fn put(&mut self, opt: Opt, level: OptionLevel, val: OptVal) {
        if let Some(core) = core_option(opt, &val) {
            let global = core.is_global();
            self.ws.set_option_in(core.clone(), level);
            // The trace replays the focused window, so it records a write that reaches it.
            if level != OptionLevel::Global || global {
                crate::app::dispatch::note_ex(ExCmd::Set(core), self.recorded);
            }
            return;
        }
        if window_value(self.ws.window_defaults(), opt).is_some() {
            self.ws
                .update_window_options(level, |o| set_window_value(o, opt, &val));
            return;
        }
        let on = matches!(val, OptVal::Bool(true));
        let buf = self.ws.focused_buffer();
        let text = match &val {
            OptVal::Text(t) => t.to_string(),
            _ => String::new(),
        };
        match (opt, val) {
            // The global-local display options: as 'scrolloff' (`:set` drops the window's own value).
            (Opt::ShowBreak | Opt::ListChars | Opt::FillChars | Opt::SideScrollOff, v) => {
                let n = match v {
                    OptVal::Num(n) => n,
                    _ => 0,
                };
                if level != OptionLevel::Global {
                    let local = level == OptionLevel::Local;
                    self.ws
                        .update_window_options(OptionLevel::Local, |o| match opt {
                            Opt::ShowBreak => o.showbreak = local.then(|| text.clone()),
                            Opt::ListChars => o.listchars = local.then(|| text.clone()),
                            Opt::FillChars => o.fillchars = local.then(|| text.clone()),
                            _ => o.sidescrolloff = local.then_some(n),
                        });
                }
                if level != OptionLevel::Local {
                    let d = &mut self.ui.display;
                    match opt {
                        Opt::ShowBreak => d.showbreak = text,
                        Opt::ListChars => d.listchars = text,
                        Opt::FillChars => d.fillchars = text,
                        _ => d.sidescrolloff = n,
                    }
                }
            }
            (Opt::BreakAt, _) => self.ui.display.breakat = text,
            (Opt::Display, _) => self.ui.display.display = text,
            (Opt::SideScroll, OptVal::Num(n)) => self.ui.display.sidescroll = n,
            (Opt::ScrollJump, OptVal::Num(n)) => self.ui.display.scrolljump = n,
            // 'scroll' 0 is half the window again (nvim's `win_comp_scroll`).
            (Opt::Scroll, OptVal::Num(n)) => {
                let h = self.win_height.max(1);
                let lines = usize::try_from(n).unwrap_or(0);
                let lines = if lines == 0 { (h / 2).max(1) } else { lines };
                let focus = self.ws.focus();
                self.ws.set_scroll(focus, lines, h);
            }
            (Opt::HlSearch, _) => {
                *self.hlsearch_on = on;
                if !on {
                    // Switching hlsearch off also clears the live highlight (Vim).
                    *self.search_hl = None;
                }
            }
            (Opt::IncSearch, _) => *self.incsearch_on = on,
            (Opt::FixEol, _) => self.files.write.fixeol = on,
            (Opt::TildeOp, _) => self.engine.set_tildeop(on),
            // Global-local: `:setlocal` gives the focused window its own value, `:set` sets the global
            // value and drops the window's (nvim), `:setglobal` leaves the window's alone.
            (Opt::ScrollOff, OptVal::Num(n)) => match level {
                OptionLevel::Local => self.ws.set_focused_scrolloff(Some(n)),
                OptionLevel::Both => {
                    *self.scrolloff = n;
                    self.ws.set_focused_scrolloff(None);
                }
                OptionLevel::Global => *self.scrolloff = n,
            },
            (Opt::ShowMode, _) => self.ui.showmode = on,
            (Opt::Verbose, OptVal::Num(n)) => self.ui.verbose = n,
            (Opt::MessageOptions, _) => self.ui.messagesopt = text,
            (Opt::FoldOpen, _) => self.ui.foldopen = text,
            (Opt::Report, OptVal::Num(n)) => self.ui.report = n,
            (Opt::History, OptVal::Num(n)) => {
                self.engine.set_history_len(usize::try_from(n).unwrap_or(0));
            }
            // The fold options (frontend state, per buffer). Switching the method keeps the folds and
            // recomputes them next frame (Vim keeps them); a level or enable change re-applies at once.
            (Opt::FoldMethod, OptVal::Str(m)) => {
                let m = if m == "indent" {
                    FoldMethod::Indent
                } else {
                    FoldMethod::Manual
                };
                // Folds an indent method made stay as manual ones, at their depths.
                if m == FoldMethod::Manual {
                    if let Some(fv) = self.folds.get_mut(&buf) {
                        crate::folds::renest(fv);
                    }
                }
                self.fold_method.insert(buf, m);
                self.fold_rev.remove(&buf);
            }
            (Opt::FoldLevel, OptVal::Num(n)) => {
                self.foldlevel.insert(buf, usize::try_from(n).unwrap_or(0));
                self.reapply_folds_for(buf, true);
            }
            (Opt::FoldEnable, _) => {
                self.foldenable.insert(buf, on);
                self.reapply_folds(buf);
            }
            (Opt::AutoRead, _) => self.ui.buffers.autoread = on,
            (Opt::AutoWrite, _) => self.ui.buffers.autowrite = on,
            // nvim: 'autowriteall' implies 'autowrite' (it does not set it).
            (Opt::AutoWriteAll, _) => self.ui.buffers.autowriteall = on,
            (Opt::Confirm, _) => self.ui.buffers.confirm = on,
            (Opt::Hidden, _) => self.ui.buffers.hidden = on,
            // Buffer state of the focused buffer, recorded so a replay refuses the same edits (#619).
            (Opt::Modifiable, _) => {
                crate::app::dispatch::run_ex(ExCmd::Modifiable(on), self.ws, self.recorded);
            }
            // `:set nomod` marks the buffer unchanged (its text stays); `:set mod` marks it changed.
            (Opt::Modified, _) => {
                crate::app::dispatch::run_ex(ExCmd::Modified(on), self.ws, self.recorded);
            }
            (Opt::FileFormats, _) => self.files.write.fileformats = text,
            // `'fileformat'` changes how the file is written, so the buffer is modified (nvim); `:setglobal`
            // sets the format of buffers without a file.
            (Opt::FileFormat, OptVal::Str(name)) => {
                let eol = crate::persist::encoding::Eol::from_name(name).unwrap_or_default();
                if level == OptionLevel::Global {
                    self.files.write.fileformat = eol;
                } else {
                    let before = self.file_format(buf);
                    self.set_file_format(buf, |f| f.eol = eol);
                    if before.eol != eol && !before.binary {
                        self.ws.focused_doc_mut().mark_unsaved();
                    }
                    if level == OptionLevel::Both && !self.files.contains_key(&buf) {
                        self.files.write.fileformat = eol;
                    }
                }
            }
            // nvim's `'binary'`: on, `'textwidth'` and `'wrapmargin'` go to 0 and `'expandtab'` off, and come
            // back when it goes off again.
            (Opt::Binary, _) => {
                let was = self.file_format(buf).binary;
                self.set_file_format(buf, |f| f.binary = on);
                if on && !was {
                    let num = |v: OptVal| match v {
                        OptVal::Num(n) => n,
                        _ => 0,
                    };
                    let saved = (
                        num(self.get(Opt::TextWidth, false)),
                        num(self.get(Opt::WrapMargin, false)),
                        matches!(self.get(Opt::ExpandTab, false), OptVal::Bool(true)),
                    );
                    self.files.write.bin_saved = Some(saved);
                    self.put(Opt::TextWidth, level, OptVal::Num(0));
                    self.put(Opt::WrapMargin, level, OptVal::Num(0));
                    self.put(Opt::ExpandTab, level, OptVal::Bool(false));
                } else if !on && was {
                    if let Some((tw, wm, et)) = self.files.write.bin_saved.take() {
                        self.put(Opt::TextWidth, level, OptVal::Num(tw));
                        self.put(Opt::WrapMargin, level, OptVal::Num(wm));
                        self.put(Opt::ExpandTab, level, OptVal::Bool(et));
                    }
                }
            }
            (Opt::Backup, _) => self.files.write.backup = on,
            (Opt::WriteBackup, _) => self.files.write.writebackup = on,
            (Opt::Write, _) => self.files.write.write = on,
            (Opt::WriteAny, _) => self.files.write.writeany = on,
            (Opt::Fsync, _) => self.files.write.fsync = on,
            (Opt::BackupDir, _) => self.files.write.backupdir = Some(text),
            (Opt::BackupSkip, _) => self.files.write.backupskip = Some(text),
            (Opt::BackupExt, _) => self.files.write.backupext = text,
            (Opt::BackupCopy, _) => self.files.write.backupcopy = text,
            (Opt::PatchMode, _) => self.files.write.patchmode = text,
            // The file's shape is the buffer's. Changing its BOM changes the file, so the buffer is modified
            // (nvim); `'endofline'` and `'endoffile'` do not modify it.
            (Opt::EndOfLine | Opt::EndOfFile | Opt::Bomb, _) => {
                let before = self.file_format(buf).bom;
                self.set_file_format(buf, |fmt| match opt {
                    Opt::EndOfLine => fmt.final_newline = on,
                    Opt::EndOfFile => fmt.eof = on,
                    _ => fmt.bom = on,
                });
                if opt == Opt::Bomb && before != on {
                    self.ws.focused_doc_mut().mark_unsaved();
                }
            }
            // `'readonly'` is the buffer's; a `:set noro` (not `:setlocal`) also ends `ruse -R`'s readonly
            // mode, as nvim's does.
            (Opt::ReadOnly, _) => {
                self.readonly.set(self.files, buf, on);
                if !on && level != OptionLevel::Local {
                    self.readonly.mode = false;
                }
            }
            _ => {}
        }
    }

    /// The shape of `buf`'s file (`'endofline'`, `'endoffile'`, `'bomb'`): its file's, or the one a buffer
    /// without a file will be written with.
    fn file_format(&self, buf: DocumentId) -> crate::persist::encoding::FileFormat {
        self.files.get(&buf).map(|bf| bf.fmt).unwrap_or_else(|| {
            self.files
                .write
                .nofile_fmt
                .get(&buf)
                .copied()
                .unwrap_or_else(|| self.files.write.new_format())
        })
    }

    /// Change the shape of `buf`'s file (or of the one a buffer without a file will be written with).
    fn set_file_format(
        &mut self,
        buf: DocumentId,
        f: impl FnOnce(&mut crate::persist::encoding::FileFormat),
    ) {
        let mut fmt = self.file_format(buf);
        f(&mut fmt);
        match self.files.get_mut(&buf) {
            Some(bf) => bf.fmt = fmt,
            None => {
                self.files.write.nofile_fmt.insert(buf, fmt);
            }
        }
    }

    /// Close the folds deeper than 'foldlevel' (all of them open with 'nofoldenable'). Manual folds take a
    /// new 'foldlevel' too (nvim's `newFoldLevel`), but not a 'foldenable' change, which would lose the
    /// state each one was left in.
    fn reapply_folds(&mut self, buf: DocumentId) {
        self.reapply_folds_for(buf, false);
    }

    fn reapply_folds_for(&mut self, buf: DocumentId, level_changed: bool) {
        if self.fold_method.get(&buf).copied() != Some(FoldMethod::Indent) && !level_changed {
            return;
        }
        if let Some(fv) = self.folds.get_mut(&buf) {
            let level = if self.foldenable.get(&buf).copied().unwrap_or(true) {
                self.foldlevel.get(&buf).copied().unwrap_or(0)
            } else {
                usize::MAX
            };
            crate::folds::apply_foldlevel(fv, level);
        }
    }

    /// The `Last set from` line `:verbose set opt?` adds when a config file set the value.
    fn last_set(&self, opt: Opt) -> Option<String> {
        match self.sources.get(&opt)? {
            OptSource::Typed => None,
            OptSource::Config { layer, file, line } => Some(format!(
                "\tLast set from {file} line {line} ({layer} config)"
            )),
        }
    }
}

fn level(scope: SetScope) -> OptionLevel {
    match scope {
        SetScope::Both => OptionLevel::Both,
        SetScope::Local => OptionLevel::Local,
        SetScope::Global => OptionLevel::Global,
    }
}

/// Run a parsed `:set` line in order, stopping at the first error (nvim's `do_set`). `cols` is the
/// screen width a listing lays out in; `verbose` is the `:verbose` modifier.
pub(crate) fn run_set(
    line: &SetLine,
    host: &mut OptionHost<'_>,
    cols: usize,
    verbose: bool,
) -> SetOutcome {
    let mut out = SetOutcome::default();
    let global = line.scope == SetScope::Global;
    let lvl = level(line.scope);
    if line.args.is_empty() && line.error.is_none() {
        let items: Vec<(&OptDef, OptVal)> = OPTIONS
            .iter()
            .map(|d| (d, host.read(d.opt, line.scope)))
            // 'scroll' follows the window's height; it lists only as `:set all` shows it.
            .filter(|(d, v)| *v != default_of(d) && d.opt != Opt::Scroll)
            .collect();
        out.rows = listing(title(line.scope), &items, cols);
        return out;
    }
    let mut notice = None;
    for (arg, text) in &line.args {
        let write = |host: &mut OptionHost<'_>, opt: Opt, val: OptVal| {
            host.put(opt, lvl, val);
            host.sources.insert(opt, OptSource::Typed);
        };
        match arg {
            SetArg::Show(opt) => {
                out.rows
                    .push(show_one(opt_def(*opt), host.read(*opt, line.scope)));
                if verbose || host.ui.verbose > 0 {
                    out.rows.extend(host.last_set(*opt));
                }
            }
            SetArg::Bool(opt, on) => write(host, *opt, OptVal::Bool(*on)),
            SetArg::Toggle(opt) => {
                let on = matches!(host.get(*opt, global), OptVal::Bool(true));
                write(host, *opt, OptVal::Bool(!on));
            }
            SetArg::Default { opt, vim } => {
                let def = opt_def(*opt);
                write(
                    host,
                    *opt,
                    if *vim {
                        vim_default_of(def)
                    } else {
                        default_of(def)
                    },
                );
            }
            SetArg::FromGlobal(opt) => {
                if matches!(
                    opt_def(*opt).locality,
                    Locality::Local | Locality::GlobalLocal
                ) {
                    let g = host.get(*opt, true);
                    host.put(*opt, OptionLevel::Local, g);
                    host.sources.insert(*opt, OptSource::Typed);
                }
            }
            SetArg::Assign { opt, op, val } => {
                let def = opt_def(*opt);
                let value = assign_value(def, host.read(*opt, line.scope), *op, val);
                // 'scroll' above the window's height is nvim's E49.
                let value = match value {
                    Ok(OptVal::Num(n))
                        if *opt == Opt::Scroll
                            && host.win_height > 0
                            && usize::try_from(n).unwrap_or(usize::MAX) > host.win_height =>
                    {
                        Err(crate::input::set_cmd::ValueError::Message(
                            "E49: Invalid scroll size".into(),
                        ))
                    }
                    // nvim: `'backupext'` and `'patchmode'` may not be equal (E589).
                    Ok(OptVal::Text(t))
                        if (*opt == Opt::BackupExt && *t == host.files.write.patchmode)
                            || (*opt == Opt::PatchMode && *t == host.files.write.backupext) =>
                    {
                        Err(crate::input::set_cmd::ValueError::Message(
                            "E589: 'backupext' and 'patchmode' are equal".into(),
                        ))
                    }
                    v => v,
                };
                match value {
                    Ok(OptVal::Str(s)) if !supported(def, s) => {
                        notice = Some(format!("option not supported: {}", text.trim_end()));
                    }
                    Ok(OptVal::Text(t)) if !text_supported(*opt, &t) => {
                        notice = Some(format!("option not supported: {}", text.trim_end()));
                    }
                    Ok(v) => write(host, *opt, v),
                    Err(e) => {
                        out.error = Some(format!("{}: {text}", e.message()));
                        return out;
                    }
                }
            }
            SetArg::ShowAll => {
                let items: Vec<(&OptDef, OptVal)> = OPTIONS
                    .iter()
                    .map(|d| (d, host.read(d.opt, line.scope)))
                    .collect();
                out.rows.extend(listing(title(line.scope), &items, cols));
            }
            SetArg::ResetAll => {
                for d in OPTIONS {
                    write(host, d.opt, default_of(d));
                }
            }
            SetArg::Unsupported(_) => {
                notice = Some(format!("option not supported: {}", text.trim_end()));
            }
            SetArg::Ignored => {}
        }
    }
    out.error = line.error.clone();
    if out.error.is_none() && out.rows.is_empty() {
        out.rows.extend(notice);
    }
    out
}

/// ruse's default of `def`: the table's, except for the options whose default depends on the environment
/// (`'backupdir'` names the state directory, `'backupskip'` the temp directories).
fn default_of(def: &OptDef) -> OptVal {
    match def.opt {
        Opt::BackupDir => OptVal::Text(crate::persist::backup::default_backupdir().into()),
        Opt::BackupSkip => OptVal::Text(crate::persist::backup::default_backupskip().into()),
        _ => def.default.clone(),
    }
}

/// nvim's default of `def` (`opt&vim`), environment-dependent as [`default_of`].
fn vim_default_of(def: &OptDef) -> OptVal {
    match def.opt {
        Opt::BackupDir | Opt::BackupSkip => default_of(def),
        _ => def.vim_default.clone(),
    }
}

fn supported(def: &OptDef, value: &str) -> bool {
    match def.kind {
        OptKind::Str { supported, .. } => supported.contains(&value),
        _ => true,
    }
}

/// Whether ruse implements the string value `t` of `opt`: `'cpoptions'` only at nvim's default and
/// `'verbosefile'` only empty (#1151; see `input/set_cmd.rs`).
fn text_supported(opt: Opt, t: &str) -> bool {
    match opt {
        Opt::CpOptions => t == crate::input::set_cmd::CPO_DEFAULT,
        Opt::VerboseFile => t.is_empty(),
        // `'breakindentopt'` `list:` is not drawn (#615).
        Opt::BreakIndentOptions => {
            crate::ui::line_layout::parse_briopt(t).is_ok_and(|o| o.list == 0)
        }
        _ => true,
    }
}

fn title(scope: SetScope) -> &'static str {
    match scope {
        SetScope::Both => "--- Options ---",
        SetScope::Local => "--- Local option values ---",
        SetScope::Global => "--- Global option values ---",
    }
}

/// The config key an option persists as (`spec/config-schema.yaml`), if any.
#[must_use]
pub(crate) fn config_key(opt: Opt) -> Option<&'static str> {
    Some(match opt {
        Opt::TabStop => "editor.tab_width",
        Opt::ShiftWidth => "editor.shift_width",
        Opt::ExpandTab => "editor.indent_style",
        Opt::TextWidth => "editor.textwidth",
        Opt::ScrollOff => "editor.scrolloff",
        Opt::FoldOpen => "editor.foldopen",
        Opt::IgnoreCase => "editor.ignorecase",
        Opt::SmartCase => "editor.smartcase",
        Opt::HlSearch => "editor.hlsearch",
        Opt::IncSearch => "editor.incsearch",
        Opt::TildeOp => "editor.tildeop",
        Opt::FixEol => "editor.fixeol",
        Opt::Hidden => "editor.hidden",
        Opt::Confirm => "editor.confirm",
        Opt::AutoWrite => "editor.autowrite",
        Opt::AutoWriteAll => "editor.autowriteall",
        Opt::AutoRead => "editor.autoread",
        Opt::FileFormats => "files.fileformats",
        Opt::Backup => "files.backup",
        Opt::WriteBackup => "files.writebackup",
        Opt::BackupDir => "files.backupdir",
        Opt::BackupExt => "files.backupext",
        Opt::BackupCopy => "files.backupcopy",
        Opt::BackupSkip => "files.backupskip",
        Opt::PatchMode => "files.patchmode",
        Opt::Write => "files.write",
        Opt::WriteAny => "files.writeany",
        Opt::Fsync => "files.fsync",
        Opt::AutoIndent => "editor.autoindent",
        Opt::SmartIndent => "editor.smartindent",
        Opt::CopyIndent => "editor.copyindent",
        Opt::PreserveIndent => "editor.preserveindent",
        Opt::SoftTabStop => "editor.softtabstop",
        Opt::SmartTab => "editor.smarttab",
        Opt::ShiftRound => "editor.shiftround",
        Opt::WrapMargin => "editor.wrapmargin",
        Opt::FormatOptions => "editor.formatoptions",
        Opt::Comments => "editor.comments",
        Opt::CommentString => "editor.commentstring",
        Opt::FormatListPat => "editor.formatlistpat",
        Opt::FormatPrg => "editor.formatprg",
        Opt::EqualPrg => "editor.equalprg",
        Opt::CinWords => "editor.cinwords",
        Opt::Lisp => "editor.lisp",
        Opt::LispWords => "editor.lispwords",
        Opt::CIndent => "editor.cindent",
        Opt::VarTabStop => "editor.vartabstop",
        Opt::VarSoftTabStop => "editor.varsofttabstop",
        Opt::CinOptions => "editor.cinoptions",
        Opt::CinKeys => "editor.cinkeys",
        Opt::CinScopeDecls => "editor.cinscopedecls",
        Opt::JoinSpaces => "editor.joinspaces",
        Opt::SideScroll => "editor.sidescroll",
        Opt::SideScrollOff => "editor.sidescrolloff",
        Opt::Wrap => "render.wrap",
        Opt::LineBreak => "render.linebreak",
        Opt::BreakIndent => "render.breakindent",
        Opt::ShowBreak => "render.showbreak",
        Opt::Number => "render.number",
        Opt::RelativeNumber => "render.relativenumber",
        Opt::NumberWidth => "render.numberwidth",
        Opt::SignColumn => "render.signcolumn",
        Opt::List => "render.list",
        Opt::ListChars => "render.listchars",
        Opt::CursorLine => "render.cursorline",
        Opt::CursorLineOptions => "render.cursorlineopt",
        Opt::CursorColumn => "render.cursorcolumn",
        Opt::ColorColumn => "render.colorcolumn",
        Opt::FoldColumn => "render.foldcolumn",
        Opt::FillChars => "render.fillchars",
        Opt::SmoothScroll => "render.smoothscroll",
        _ => return None,
    })
}

/// A config value as the option's value (`editor.indent_style = "space"` is `'expandtab'`). A string
/// option's value passes the check `:set` gives it (`editor.formatoptions = "xyz"` is ignored, E539).
fn option_value(opt: Opt, v: &crate::config::Value) -> Option<OptVal> {
    use crate::config::Value;
    match (opt, v) {
        (Opt::ExpandTab, Value::Str(s)) => Some(OptVal::Bool(s == "space")),
        (_, Value::Str(s)) if matches!(opt_def(opt).kind, OptKind::Text { .. }) => {
            let def = opt_def(opt);
            let raw = crate::input::set_cmd::RawVal::Str(s.clone());
            assign_value(
                def,
                def.default.clone(),
                crate::input::set_cmd::AssignOp::Set,
                &raw,
            )
            .ok()
        }
        (_, Value::Bool(b)) => Some(OptVal::Bool(*b)),
        (_, Value::Int(n)) => Some(OptVal::Num(*n)),
        _ => None,
    }
}

/// Install the user config's `keymap.lang` table into the Lang-Arg map (#1151, F-027), the persistent
/// form of `:lmap`: each `lhs = "rhs"` entry maps one character to one character. `installed` holds the
/// pairs an earlier config installed; a reload first takes back those still in place (a `:lmap` typed
/// since keeps its own). Returns a note for each entry it skipped. Only the machine and user layers can
/// set the key (`scope: user`, D-062), so a workspace never binds keys here.
pub(crate) fn install_lang_keymap(
    cfg: &crate::config::Config,
    engine: &mut InputEngine,
    installed: &mut Vec<(char, char)>,
) -> Vec<String> {
    use crate::config::Value;
    for (lhs, rhs) in installed.drain(..) {
        if engine.lang_mapping(lhs) == Some(rhs) {
            engine.clear_lang_mapping(lhs);
        }
    }
    let Some(Value::Map(map)) = cfg.get("keymap.lang").map(|e| &e.value) else {
        return Vec::new();
    };
    let one = |s: &str| {
        let mut c = s.chars();
        c.next().filter(|_| c.next().is_none())
    };
    let mut notes = Vec::new();
    for (lhs, rhs) in map {
        match (one(lhs), rhs) {
            (Some(l), Value::Str(r)) if one(r).is_some() => {
                let r = one(r).expect("checked");
                engine.set_lang_mapping(l, r);
                installed.push((l, r));
            }
            _ => notes.push(format!(
                "keymap.lang: {lhs:?} skipped (a mapping is one character to one character)"
            )),
        }
    }
    notes
}

/// Install `cfg`'s values into the options (`:set` writes both copies), with their provenance. An option
/// typed with `:set` this session is left alone, and one an earlier config set but `cfg` no longer does
/// (`:untrust` made the workspace layer inert) goes back to ruse's default. With `only`, just the options of
/// those config keys (a reload installs what changed).
pub(crate) fn apply_config_keys(
    cfg: &crate::config::Config,
    host: &mut OptionHost<'_>,
    only: Option<&[&'static str]>,
) {
    // The Emacs profile's file variables are config keys alone (`emacs.*`): ruse has no `set-variable`.
    if host.files.write.emacs.is_some()
        && only.is_none_or(|keys| keys.iter().any(|k| k.starts_with("emacs.")))
    {
        host.files.write.emacs = Some(crate::app::write_opts::EmacsFiles::from_config(cfg));
    }
    // `render.conceal` is no `:set` option: the config's value, else the schema default (on).
    if only.is_none_or(|keys| keys.contains(&"render.conceal")) {
        host.ui.conceal = !matches!(
            cfg.get("render.conceal").map(|e| &e.value),
            Some(crate::config::Value::Bool(false))
        );
    }
    for def in OPTIONS {
        let Some(key) = config_key(def.opt) else {
            continue;
        };
        if only.is_some_and(|keys| !keys.contains(&key)) {
            continue;
        }
        if host.sources.get(&def.opt) == Some(&OptSource::Typed) {
            continue;
        }
        let found = cfg
            .get(key)
            .and_then(|e| Some((e, option_value(def.opt, &e.value)?)));
        match found {
            Some((e, v)) => {
                host.put(def.opt, OptionLevel::Both, v);
                host.sources.insert(
                    def.opt,
                    OptSource::Config {
                        layer: e.source.layer.label(),
                        file: crate::config::short_path(&e.source.path),
                        line: e.source.line,
                    },
                );
            }
            None => {
                if host.sources.remove(&def.opt).is_some() {
                    host.put(def.opt, OptionLevel::Both, default_of(def));
                }
            }
        }
    }
}
