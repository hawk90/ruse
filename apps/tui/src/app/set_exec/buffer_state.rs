//! The buffer-state family: the buffer-abandon options (#619) and the focused buffer's own
//! `'modifiable'`, `'modified'` and `'readonly'`.

use ruse_core::{DocumentId, ExCmd, OptionLevel};

use super::OptionHost;
use crate::input::set_cmd::{Opt, OptVal};

impl OptionHost<'_> {
    /// The value of a buffer-state option for `buf`.
    pub(super) fn get_buffer_state(&self, opt: Opt, buf: DocumentId) -> OptVal {
        match opt {
            Opt::ReadOnly => OptVal::Bool(self.readonly.get(self.files, buf)),
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
            _ => unreachable!(),
        }
    }

    /// The buffer-abandon options (#619) and the focused buffer's own state.
    pub(super) fn put_buffer_state(
        &mut self,
        opt: Opt,
        level: OptionLevel,
        on: bool,
        buf: DocumentId,
    ) {
        match opt {
            Opt::AutoRead => self.ui.buffers.autoread = on,
            Opt::AutoWrite => self.ui.buffers.autowrite = on,
            // nvim: 'autowriteall' implies 'autowrite' (it does not set it).
            Opt::AutoWriteAll => self.ui.buffers.autowriteall = on,
            Opt::Confirm => self.ui.buffers.confirm = on,
            Opt::Hidden => self.ui.buffers.hidden = on,
            // Buffer state of the focused buffer, recorded so a replay refuses the same edits (#619).
            Opt::Modifiable => {
                crate::app::dispatch::run_ex(ExCmd::Modifiable(on), self.ws, self.recorded);
            }
            // `:set nomod` marks the buffer unchanged (its text stays); `:set mod` marks it changed.
            Opt::Modified => {
                crate::app::dispatch::run_ex(ExCmd::Modified(on), self.ws, self.recorded);
            }
            // `'readonly'` is the buffer's; a `:set noro` (not `:setlocal`) also ends `ruse -R`'s readonly
            // mode, as nvim's does.
            Opt::ReadOnly => {
                self.readonly.set(self.files, buf, on);
                if !on && level != OptionLevel::Local {
                    self.readonly.mode = false;
                }
            }
            _ => {}
        }
    }
}
