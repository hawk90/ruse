//! Running a parsed `:set` / `:setlocal` / `:setglobal` line (#614) against the option values the
//! session and the core hold, and the provenance each value carries (`:verbose set opt?`).
//!
//! The grammar and the option table are `input/set_cmd.rs`; this module owns where each value lives:
//! the core's View options (`ic`, `scs`, `ts`, `sw`, `et`, `tw`, `sts`, `ai`, `si`, `ci`, `pi`, `sta`, `sr`,
//! local per View plus the Workspace's global copy), the session's render and write preferences (`hls`, `is`, `fixeol`, `so`), the input engine's
//! `'tildeop'`, and the per-buffer fold state (`fen`, `fdm`, `fdl`).
//!
//! This file keeps the dispatch (`OptionHost::get` / `read` / `put`) and the `:set` line runner; each
//! option family's reader and installer lives in a child: [`values`] (core and window-local display
//! options), [`display`], [`session_ui`], [`fold`], [`buffer_state`], [`write`], [`file_shape`], and
//! [`config`] (installing a config file's values).

mod buffer_state;
mod config;
mod display;
mod file_shape;
mod fold;
mod session_ui;
mod values;
mod write;

use std::collections::HashMap;

use ruse_core::{Command, DocumentId, ExCmd, OptionLevel, Revision, Workspace};

use crate::input::set_cmd::{
    assign_value, listing, opt_def, show_one, AssignOp, Locality, Opt, OptDef, OptKind, OptVal,
    RawVal, SetArg, SetLine, SetScope, OPTIONS,
};
use crate::input::{FoldMethod, InputEngine};
use crate::ui::window_view::DisplayGlobals;

pub(crate) use config::{apply_config_keys, install_lang_keymap};
use values::{core_option, core_value, set_window_value, window_value};

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

/// What a `:set` line printed: the rows (`:set ts?`, listings) and the error that stopped it, if any.
#[derive(Default, Debug)]
pub(crate) struct SetOutcome {
    pub rows: Vec<String>,
    pub error: Option<String>,
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
        let local = (!global).then(|| self.ws.focused().view.window_options());
        let buf = self.ws.focused_buffer();
        match opt {
            Opt::ShowBreak
            | Opt::ListChars
            | Opt::FillChars
            | Opt::Scroll
            | Opt::SideScrollOff
            | Opt::BreakAt
            | Opt::Display
            | Opt::SideScroll
            | Opt::ScrollJump
            | Opt::ScrollOff => self.get_display(opt, global, local),
            Opt::HlSearch
            | Opt::IncSearch
            | Opt::TildeOp
            | Opt::ShowMode
            | Opt::Verbose
            | Opt::MessageOptions
            | Opt::FoldOpen
            | Opt::Report
            | Opt::History
            | Opt::CpOptions
            | Opt::VerboseFile => self.get_session_ui(opt),
            Opt::FoldEnable | Opt::FoldLevel | Opt::FoldMethod => self.get_fold(opt, buf),
            Opt::AutoRead
            | Opt::AutoWrite
            | Opt::AutoWriteAll
            | Opt::Confirm
            | Opt::Hidden
            | Opt::Modifiable
            | Opt::Modified
            | Opt::ReadOnly => self.get_buffer_state(opt, buf),
            Opt::FixEol
            | Opt::FileFormats
            | Opt::SwapFile
            | Opt::UpdateCount
            | Opt::Directory
            | Opt::Backup
            | Opt::WriteBackup
            | Opt::Write
            | Opt::WriteAny
            | Opt::Fsync
            | Opt::BackupDir
            | Opt::BackupSkip
            | Opt::BackupExt
            | Opt::BackupCopy
            | Opt::PatchMode => self.get_write(opt, buf),
            Opt::FileFormat | Opt::Binary | Opt::EndOfLine | Opt::EndOfFile | Opt::Bomb => {
                self.get_file_shape(opt, buf)
            }
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

    /// Write `val` into `opt` at `level`: a core option or a window-local display option directly, any
    /// other option through the installer of the owner that holds it.
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
        match opt {
            Opt::ShowBreak
            | Opt::ListChars
            | Opt::FillChars
            | Opt::SideScrollOff
            | Opt::BreakAt
            | Opt::Display
            | Opt::SideScroll
            | Opt::ScrollJump
            | Opt::Scroll
            | Opt::ScrollOff => self.put_display(opt, level, val, text),
            Opt::HlSearch
            | Opt::IncSearch
            | Opt::TildeOp
            | Opt::ShowMode
            | Opt::Verbose
            | Opt::MessageOptions
            | Opt::FoldOpen
            | Opt::Report
            | Opt::History => self.put_session_ui(opt, val, on, text),
            Opt::FoldMethod | Opt::FoldLevel | Opt::FoldEnable => {
                self.put_fold(opt, val, on, buf);
            }
            Opt::AutoRead
            | Opt::AutoWrite
            | Opt::AutoWriteAll
            | Opt::Confirm
            | Opt::Hidden
            | Opt::Modifiable
            | Opt::Modified
            | Opt::ReadOnly => self.put_buffer_state(opt, level, on, buf),
            Opt::FixEol
            | Opt::FileFormats
            | Opt::SwapFile
            | Opt::UpdateCount
            | Opt::Backup
            | Opt::WriteBackup
            | Opt::Write
            | Opt::WriteAny
            | Opt::Fsync
            | Opt::BackupDir
            | Opt::BackupSkip
            | Opt::BackupExt
            | Opt::BackupCopy
            | Opt::PatchMode => self.put_write(opt, level, val, on, text, buf),
            Opt::FileFormat | Opt::Binary | Opt::EndOfLine | Opt::EndOfFile | Opt::Bomb => {
                self.put_file_shape(opt, level, val, on, buf);
            }
            _ => {}
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
        match arg {
            SetArg::Show(opt) => {
                out.rows
                    .push(show_one(opt_def(*opt), host.read(*opt, line.scope)));
                if verbose || host.ui.verbose > 0 {
                    out.rows.extend(host.last_set(*opt));
                }
            }
            SetArg::Bool(opt, on) => write_typed(host, lvl, *opt, OptVal::Bool(*on)),
            SetArg::Toggle(opt) => {
                let on = matches!(host.get(*opt, global), OptVal::Bool(true));
                write_typed(host, lvl, *opt, OptVal::Bool(!on));
            }
            SetArg::Default { opt, vim } => {
                let def = opt_def(*opt);
                write_typed(
                    host,
                    lvl,
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
                match assign(host, line.scope, lvl, *opt, *op, val, text) {
                    Ok(Some(n)) => notice = Some(n),
                    Ok(None) => {}
                    Err(e) => {
                        out.error = Some(e);
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
                    write_typed(host, lvl, d.opt, default_of(d));
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

/// Write `val` into `opt` at `lvl` as typed (`:set`): a config reload leaves it alone.
fn write_typed(host: &mut OptionHost<'_>, lvl: OptionLevel, opt: Opt, val: OptVal) {
    host.put(opt, lvl, val);
    host.sources.insert(opt, OptSource::Typed);
}

/// One `opt=val` (`+=`, `-=`, `^=`) argument of a `:set` line at `scope`: `Ok(None)` once the value is
/// written at `lvl`, `Ok(Some(notice))` when ruse does not support the value, `Err` with the error that
/// stops the line.
fn assign(
    host: &mut OptionHost<'_>,
    scope: SetScope,
    lvl: OptionLevel,
    opt: Opt,
    op: AssignOp,
    val: &RawVal,
    text: &str,
) -> Result<Option<String>, String> {
    let def = opt_def(opt);
    let value = assign_value(def, host.read(opt, scope), op, val);
    // 'scroll' above the window's height is nvim's E49.
    let value = match value {
        Ok(OptVal::Num(n))
            if opt == Opt::Scroll
                && host.win_height > 0
                && usize::try_from(n).unwrap_or(usize::MAX) > host.win_height =>
        {
            Err(crate::input::set_cmd::ValueError::Message(
                "E49: Invalid scroll size".into(),
            ))
        }
        // nvim: `'backupext'` and `'patchmode'` may not be equal (E589).
        Ok(OptVal::Text(t))
            if (opt == Opt::BackupExt && *t == host.files.write.patchmode)
                || (opt == Opt::PatchMode && *t == host.files.write.backupext) =>
        {
            Err(crate::input::set_cmd::ValueError::Message(
                "E589: 'backupext' and 'patchmode' are equal".into(),
            ))
        }
        v => v,
    };
    match value {
        Ok(OptVal::Str(s)) if !supported(def, s) => {
            Ok(Some(format!("option not supported: {}", text.trim_end())))
        }
        Ok(OptVal::Text(t)) if !text_supported(opt, &t) => {
            Ok(Some(format!("option not supported: {}", text.trim_end())))
        }
        Ok(v) => {
            write_typed(host, lvl, opt, v);
            Ok(None)
        }
        Err(e) => Err(format!("{}: {text}", e.message())),
    }
}

/// `'directory'`'s value: the recovery journal directory (`$XDG_STATE_HOME/ruse/journal`), with nvim's `//`
/// (the journal names its file after the whole path); empty without a state directory.
fn journal_dir() -> String {
    crate::persist::journal::current()
        .map(|j| format!("{}//", j.dir().display()))
        .unwrap_or_default()
}

/// ruse's default of `def`: the table's, except for the options whose default depends on the environment
/// (`'backupdir'` names the state directory, `'backupskip'` the temp directories).
fn default_of(def: &OptDef) -> OptVal {
    match def.opt {
        Opt::BackupDir => OptVal::Text(crate::persist::backup::default_backupdir().into()),
        Opt::Directory => OptVal::Text(journal_dir().into()),
        Opt::BackupSkip => OptVal::Text(crate::persist::backup::default_backupskip().into()),
        _ => def.default.clone(),
    }
}

/// nvim's default of `def` (`opt&vim`), environment-dependent as [`default_of`].
fn vim_default_of(def: &OptDef) -> OptVal {
    match def.opt {
        Opt::BackupDir | Opt::BackupSkip | Opt::Directory => default_of(def),
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
        // The journal directory is where the next session looks for unsaved work: it is not moved (#619).
        Opt::Directory => t == journal_dir(),
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
