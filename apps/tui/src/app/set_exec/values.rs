//! The value maps between `:set` options and their two direct owners: the core's editing options
//! ([`EditorOption`], local per View plus the Workspace's global copy) and the window-local display
//! options ([`WindowOptions`], #615, #616).

use ruse_core::editor::EditorOption;
use ruse_core::WindowOptions;

use crate::input::set_cmd::{Opt, OptVal};

/// A window-local display option's value in `o` (#615, #616), `None` for any other option.
pub(super) fn window_value(o: &WindowOptions, opt: Opt) -> Option<OptVal> {
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
pub(super) fn set_window_value(o: &mut WindowOptions, opt: Opt, val: &OptVal) {
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

pub(super) fn core_option(opt: Opt, val: &OptVal) -> Option<EditorOption> {
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

pub(super) fn core_value(opts: &[EditorOption], opt: Opt) -> Option<OptVal> {
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
