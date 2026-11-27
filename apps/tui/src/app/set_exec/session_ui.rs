//! The session-UI family: search highlighting, the input engine's options and the message options.

use super::OptionHost;
use crate::input::set_cmd::{Opt, OptVal};

impl OptionHost<'_> {
    /// The value of a session-UI option.
    pub(super) fn get_session_ui(&self, opt: Opt) -> OptVal {
        match opt {
            Opt::HlSearch => OptVal::Bool(*self.hlsearch_on),
            Opt::IncSearch => OptVal::Bool(*self.incsearch_on),
            Opt::TildeOp => OptVal::Bool(self.engine.tildeop()),
            Opt::ShowMode => OptVal::Bool(self.ui.showmode),
            Opt::Verbose => OptVal::Num(self.ui.verbose),
            Opt::MessageOptions => OptVal::Text(self.ui.messagesopt.clone().into()),
            Opt::FoldOpen => OptVal::Text(self.ui.foldopen.clone().into()),
            Opt::Report => OptVal::Num(self.ui.report),
            Opt::UndoLevels => OptVal::Num(self.ui.undolevels),
            Opt::History => {
                OptVal::Num(i64::try_from(self.engine.history_len()).unwrap_or(i64::MAX))
            }
            Opt::CpOptions => OptVal::Text(crate::input::set_cmd::CPO_DEFAULT.into()),
            Opt::VerboseFile => OptVal::Text("".into()),
            _ => unreachable!(),
        }
    }

    /// The session's search, input-engine and message options.
    pub(super) fn put_session_ui(&mut self, opt: Opt, val: OptVal, on: bool, text: String) {
        match (opt, val) {
            (Opt::HlSearch, _) => {
                *self.hlsearch_on = on;
                if !on {
                    // Switching hlsearch off also clears the live highlight (Vim).
                    *self.search_hl = None;
                }
            }
            (Opt::IncSearch, _) => *self.incsearch_on = on,
            (Opt::TildeOp, _) => self.engine.set_tildeop(on),
            (Opt::ShowMode, _) => self.ui.showmode = on,
            (Opt::Verbose, OptVal::Num(n)) => self.ui.verbose = n,
            (Opt::MessageOptions, _) => self.ui.messagesopt = text,
            (Opt::FoldOpen, _) => self.ui.foldopen = text,
            (Opt::Report, OptVal::Num(n)) => self.ui.report = n,
            // Every buffer's undo bound; the states past it go at each buffer's next change (nvim).
            (Opt::UndoLevels, OptVal::Num(n)) => {
                self.ui.undolevels = n;
                self.ws.set_undo_limits(self.ui.undo_limits());
            }
            (Opt::History, OptVal::Num(n)) => {
                self.engine.set_history_len(usize::try_from(n).unwrap_or(0));
            }
            _ => {}
        }
    }
}
