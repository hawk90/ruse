//! Installing a config file's values into the options (`:set` writes both copies) with their
//! provenance, the config key each option persists as, and the `keymap.lang` table (#1151, F-027).

use ruse_core::OptionLevel;

use super::{default_of, OptSource, OptionHost};
use crate::input::set_cmd::{assign_value, opt_def, Opt, OptKind, OptVal, OPTIONS};
use crate::input::InputEngine;

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
        Opt::WrapScan => "editor.wrapscan",
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
        Opt::SwapFile => "files.swapfile",
        Opt::UpdateCount => "files.updatecount",
        Opt::UndoLevels => "editor.undolevels",
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
    // `editor.undo_max_bytes` is no `:set` option (ruse's own, RFC-0026 §1): the config's value, else
    // the schema default. A value below 0 is no byte bound, as 0 is.
    if only.is_none_or(|keys| keys.contains(&"editor.undo_max_bytes")) {
        host.ui.undo_max_bytes = match cfg.get("editor.undo_max_bytes").map(|e| &e.value) {
            Some(crate::config::Value::Int(n)) => *n,
            _ => super::UNDO_MAX_BYTES_DEFAULT,
        };
        host.ws.set_undo_limits(host.ui.undo_limits());
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
