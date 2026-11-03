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
    pub fixeol_on: &'a mut bool,
    pub scrolloff: &'a mut i64,
    pub folds: &'a mut HashMap<DocumentId, Vec<crate::folds::Fold>>,
    pub fold_method: &'a mut HashMap<DocumentId, FoldMethod>,
    pub foldlevel: &'a mut HashMap<DocumentId, usize>,
    pub foldenable: &'a mut HashMap<DocumentId, bool>,
    pub fold_rev: &'a mut HashMap<DocumentId, Revision>,
    pub sources: &'a mut HashMap<Opt, OptSource>,
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
        Opt::SoftTabStop => EditorOption::SoftTabStop(match val {
            OptVal::Num(n) => n,
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
    opts.iter().find_map(|o| match (opt, *o) {
        (Opt::IgnoreCase, EditorOption::IgnoreCase(v))
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
            .map(|d| (d, host.get(d.opt, global)))
            .filter(|(d, v)| *v != d.default)
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
                    .push(show_one(opt_def(*opt), host.get(*opt, global)));
                if verbose {
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
                write(host, *opt, if *vim { def.vim_default } else { def.default });
            }
            SetArg::FromGlobal(opt) => {
                if opt_def(*opt).locality == Locality::Local {
                    let g = host.get(*opt, true);
                    host.put(*opt, OptionLevel::Local, g);
                    host.sources.insert(*opt, OptSource::Typed);
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

/// The config key an option persists as (`spec/config-schema.yaml`), if any.
#[must_use]
pub(crate) fn config_key(opt: Opt) -> Option<&'static str> {
    Some(match opt {
        Opt::TabStop => "editor.tab_width",
        Opt::ShiftWidth => "editor.shift_width",
        Opt::ExpandTab => "editor.indent_style",
        Opt::TextWidth => "editor.textwidth",
        Opt::ScrollOff => "editor.scrolloff",
        Opt::IgnoreCase => "editor.ignorecase",
        Opt::SmartCase => "editor.smartcase",
        Opt::HlSearch => "editor.hlsearch",
        Opt::IncSearch => "editor.incsearch",
        Opt::TildeOp => "editor.tildeop",
        Opt::FixEol => "editor.fixeol",
        Opt::AutoIndent => "editor.autoindent",
        Opt::SmartIndent => "editor.smartindent",
        Opt::CopyIndent => "editor.copyindent",
        Opt::PreserveIndent => "editor.preserveindent",
        Opt::SoftTabStop => "editor.softtabstop",
        Opt::SmartTab => "editor.smarttab",
        Opt::ShiftRound => "editor.shiftround",
        _ => return None,
    })
}

/// A config value as the option's value (`editor.indent_style = "space"` is `'expandtab'`).
fn option_value(opt: Opt, v: &crate::config::Value) -> Option<OptVal> {
    use crate::config::Value;
    match (opt, v) {
        (Opt::ExpandTab, Value::Str(s)) => Some(OptVal::Bool(s == "space")),
        (_, Value::Bool(b)) => Some(OptVal::Bool(*b)),
        (_, Value::Int(n)) => Some(OptVal::Num(*n)),
        _ => None,
    }
}

/// Install `cfg`'s values into the options (`:set` writes both copies), with their provenance. An option
/// typed with `:set` this session is left alone, and one an earlier config set but `cfg` no longer does
/// (`:untrust` made the workspace layer inert) goes back to ruse's default.
pub(crate) fn apply_config(cfg: &crate::config::Config, host: &mut OptionHost<'_>) {
    for def in OPTIONS {
        let Some(key) = config_key(def.opt) else {
            continue;
        };
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
                    host.put(def.opt, OptionLevel::Both, def.default);
                }
            }
        }
    }
}
