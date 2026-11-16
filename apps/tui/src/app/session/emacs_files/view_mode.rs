//! `read-only-mode` (`C-x C-q`) and the View mode `view-read-only` enters (#619, #1503): the keys
//! `view-mode-map` binds.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ruse_core::Command;

use super::plain;
use crate::app::session::{FrameCtx, Session};

impl Session {
    /// `read-only-mode` (`C-x C-q`): toggle `buffer-read-only`; with `view-read-only`, turning it on enters
    /// View mode and turning it off leaves it.
    pub(super) fn read_only_mode(&mut self) {
        let id = self.ws.focused_buffer();
        let on = self.ws.focused().doc.is_modifiable();
        self.ws.focused_doc_mut().set_modifiable(!on);
        if on {
            self.status.echo("Read-Only mode enabled in current buffer");
            if self.emacs_vars().view_read_only {
                self.view_mode.insert(id, true);
            }
        } else {
            self.status
                .echo("Read-Only mode disabled in current buffer");
            self.view_mode.remove(&id);
        }
    }

    /// A key in View mode (`view-mode-map`), or `None` when View mode does not bind it.
    pub(crate) fn view_key(&mut self, key: KeyEvent, frame: &FrameCtx) -> bool {
        let id = self.ws.focused_buffer();
        let Some(&from_read_only) = self.view_mode.get(&id) else {
            return false;
        };
        let rows = usize::from(frame.text_rows.max(3));
        let page = rows.saturating_sub(2).max(1);
        let half = (rows / 2).max(1);
        let shift_space = key.code == KeyCode::Char(' ') && key.modifiers == KeyModifiers::SHIFT;
        let mv = |s: &mut Session, down: bool, n: usize| {
            let cmd = if down {
                Command::MoveDown
            } else {
                Command::MoveUp
            };
            let Session {
                ws,
                files,
                recorded,
                status,
                quit,
                ..
            } = s;
            for _ in 0..n {
                crate::app::dispatch::run_cmd(cmd.clone(), ws, files, recorded, status, quit);
            }
        };
        if shift_space || key.code == KeyCode::Backspace && key.modifiers.is_empty() {
            mv(self, false, page);
        } else if plain(key, ' ') {
            mv(self, true, page);
        } else if plain(key, 'd') {
            mv(self, true, half);
        } else if plain(key, 'u') {
            mv(self, false, half);
        } else if key.code == KeyCode::Enter && key.modifiers.is_empty() {
            mv(self, true, 1);
        } else if plain(key, '<') || plain(key, '>') {
            let cmd = Command::EmacsBufferEdge {
                start: plain(key, '<'),
            };
            let Session {
                ws,
                files,
                recorded,
                status,
                quit,
                ..
            } = self;
            crate::app::dispatch::run_cmd(cmd, ws, files, recorded, status, quit);
        } else if plain(key, 'q') || plain(key, 'e') || plain(key, 'E') {
            // `q` (`View-quit`) gives a buffer View mode made read-only its writability back; `e`
            // (`View-exit`) keeps it read-only; `E` (`View-exit-and-edit`) makes it writable.
            self.view_mode.remove(&id);
            if plain(key, 'E') || (plain(key, 'q') && from_read_only) {
                self.ws.focused_doc_mut().set_modifiable(true);
            }
            self.status.clear();
        } else {
            return false;
        }
        true
    }
}
