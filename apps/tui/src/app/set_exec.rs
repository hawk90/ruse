//! Running a parsed `:set` / `:setlocal` / `:setglobal` line (#614) against the option values the
//! session and the core hold.
//!
//! The grammar and the option table are `input/set_cmd.rs`; this module owns where each value lives:
//! the core's View options (`ic`, `scs`, `ts`, `sw`, `et`, `tw`, local per View plus the Workspace's global
//! copy), the session's render and write preferences (`hls`, `is`, `fixeol`, `so`), the input engine's
//! `'tildeop'`, and the per-buffer fold state (`fen`, `fdm`, `fdl`).

use std::collections::HashMap;

use ruse_core::editor::EditorOption;
use ruse_core::{Command, DocumentId, ExCmd, OptionLevel, Revision, Workspace};

use crate::input::set_cmd::{
    assign_value, listing, opt_def, show_one, Locality, Opt, OptDef, OptKind, OptVal, SetArg,
    SetLine, SetScope, OPTIONS,
};
use crate::input::{FoldMethod, InputEngine};

/// The option values a `:set` line reads and writes, borrowed from the session for one command.
pub(crate) struct OptionHost<'a> {
    pub ws: &'a mut Workspace,
    pub recorded: &'a mut Vec<Command>,
    pub engine: &'a mut InputEngine,
    pub hlsearch_on: &'a mut bool,
    pub search_hl: &'a mut Option<String>,
    pub incsearch_on: &'a mut bool,
    pub fixeol_on: &'a mut bool,
    pub scrolloff: &'a mut i64,
    pub folds: &'a mut HashMap<DocumentId, Vec<crate::folds::Fold>>,
    pub fold_method: &'a mut HashMap<DocumentId, FoldMethod>,
    pub foldlevel: &'a mut HashMap<DocumentId, usize>,
    pub foldenable: &'a mut HashMap<DocumentId, bool>,
    pub fold_rev: &'a mut HashMap<DocumentId, Revision>,
}

/// What a `:set` line printed: the rows (`:set ts?`, listings) and the error that stopped it, if any.
#[derive(Default, Debug)]
pub(crate) struct SetOutcome {
    pub rows: Vec<String>,
    pub error: Option<String>,
}

fn core_option(opt: Opt, val: OptVal) -> Option<EditorOption> {
    let n = |v: OptVal| match v {
        OptVal::Num(n) => usize::try_from(n).unwrap_or(0),
        _ => 0,
    };
    let b = |v: OptVal| matches!(v, OptVal::Bool(true));
    Some(match opt {
        Opt::IgnoreCase => EditorOption::IgnoreCase(b(val)),
        Opt::SmartCase => EditorOption::SmartCase(b(val)),
        Opt::TabStop => EditorOption::TabStop(n(val)),
        Opt::ShiftWidth => EditorOption::ShiftWidth(n(val)),
        Opt::ExpandTab => EditorOption::ExpandTab(b(val)),
        Opt::TextWidth => EditorOption::TextWidth(n(val)),
        _ => return None,
    })
}

fn core_value(opts: &[EditorOption], opt: Opt) -> Option<OptVal> {
    opts.iter().find_map(|o| match (opt, *o) {
        (Opt::IgnoreCase, EditorOption::IgnoreCase(v))
        | (Opt::SmartCase, EditorOption::SmartCase(v))
        | (Opt::ExpandTab, EditorOption::ExpandTab(v)) => Some(OptVal::Bool(v)),
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
        let buf = self.ws.focused_buffer();
        match opt {
            Opt::HlSearch => OptVal::Bool(*self.hlsearch_on),
            Opt::IncSearch => OptVal::Bool(*self.incsearch_on),
            Opt::FixEol => OptVal::Bool(*self.fixeol_on),
            Opt::TildeOp => OptVal::Bool(self.engine.tildeop()),
            Opt::ScrollOff => OptVal::Num(*self.scrolloff),
            Opt::FoldEnable => OptVal::Bool(self.foldenable.get(&buf).copied().unwrap_or(true)),
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

    /// Write `val` into `opt` at `level`.
    pub(crate) fn put(&mut self, opt: Opt, level: OptionLevel, val: OptVal) {
        if let Some(core) = core_option(opt, val) {
            self.ws.set_option_in(core, level);
            // The trace replays the focused window, so it records a write that reaches it.
            if level != OptionLevel::Global || core.is_global() {
                crate::app::dispatch::note_ex(ExCmd::Set(core), self.recorded);
            }
            return;
        }
        let on = matches!(val, OptVal::Bool(true));
        let buf = self.ws.focused_buffer();
        match (opt, val) {
            (Opt::HlSearch, _) => {
                *self.hlsearch_on = on;
                if !on {
                    // Switching hlsearch off also clears the live highlight (Vim).
                    *self.search_hl = None;
                }
            }
            (Opt::IncSearch, _) => *self.incsearch_on = on,
            (Opt::FixEol, _) => *self.fixeol_on = on,
            (Opt::TildeOp, _) => self.engine.set_tildeop(on),
            (Opt::ScrollOff, OptVal::Num(n)) => *self.scrolloff = n,
            // The fold options (frontend state, per buffer). Switching the method keeps the folds and
            // recomputes them next frame (Vim keeps them); a level or enable change re-applies at once.
            (Opt::FoldMethod, OptVal::Str(m)) => {
                let m = if m == "indent" {
                    FoldMethod::Indent
                } else {
                    FoldMethod::Manual
                };
                self.fold_method.insert(buf, m);
                self.fold_rev.remove(&buf);
            }
            (Opt::FoldLevel, OptVal::Num(n)) => {
                self.foldlevel.insert(buf, usize::try_from(n).unwrap_or(0));
                self.reapply_folds(buf);
            }
            (Opt::FoldEnable, _) => {
                self.foldenable.insert(buf, on);
                self.reapply_folds(buf);
            }
            _ => {}
        }
    }

    fn reapply_folds(&mut self, buf: DocumentId) {
        if self.fold_method.get(&buf).copied() != Some(FoldMethod::Indent) {
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
}

fn level(scope: SetScope) -> OptionLevel {
    match scope {
        SetScope::Both => OptionLevel::Both,
        SetScope::Local => OptionLevel::Local,
        SetScope::Global => OptionLevel::Global,
    }
}

/// Run a parsed `:set` line in order, stopping at the first error (nvim's `do_set`). `cols` is the
/// screen width a listing lays out in.
pub(crate) fn run_set(line: &SetLine, host: &mut OptionHost<'_>, cols: usize) -> SetOutcome {
    let mut out = SetOutcome::default();
    let global = line.scope == SetScope::Global;
    let lvl = level(line.scope);
    if line.args.is_empty() && line.error.is_none() {
        let items: Vec<(&OptDef, OptVal)> = OPTIONS
            .iter()
            .map(|d| (d, host.get(d.opt, global)))
            .filter(|(d, v)| *v != d.default)
            .collect();
        out.rows = listing(title(line.scope), &items, cols);
        return out;
    }
    let mut notice = None;
    for (arg, text) in &line.args {
        let write = |host: &mut OptionHost<'_>, opt: Opt, val: OptVal| host.put(opt, lvl, val);
        match arg {
            SetArg::Show(opt) => {
                out.rows
                    .push(show_one(opt_def(*opt), host.get(*opt, global)));
            }
            SetArg::Bool(opt, on) => write(host, *opt, OptVal::Bool(*on)),
            SetArg::Toggle(opt) => {
                let on = matches!(host.get(*opt, global), OptVal::Bool(true));
                write(host, *opt, OptVal::Bool(!on));
            }
            SetArg::Default { opt, vim } => {
                let def = opt_def(*opt);
                write(host, *opt, if *vim { def.vim_default } else { def.default });
            }
            SetArg::FromGlobal(opt) => {
                if opt_def(*opt).locality == Locality::Local {
                    let g = host.get(*opt, true);
                    host.put(*opt, OptionLevel::Local, g);
                }
            }
            SetArg::Assign { opt, op, val } => {
                let def = opt_def(*opt);
                match assign_value(def, host.get(*opt, global), *op, val) {
                    Ok(OptVal::Str(s)) if !supported(def, s) => {
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
                    .map(|d| (d, host.get(d.opt, global)))
                    .collect();
                out.rows.extend(listing(title(line.scope), &items, cols));
            }
            SetArg::ResetAll => {
                for d in OPTIONS {
                    write(host, d.opt, d.default);
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

fn supported(def: &OptDef, value: &str) -> bool {
    match def.kind {
        OptKind::Str { supported, .. } => supported.contains(&value),
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
