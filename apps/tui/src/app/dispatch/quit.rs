//! Quit guards (E37/E162) and the exit status `:cquit` asks for.

use std::sync::atomic::{AtomicU8, Ordering};

use ruse_core::Workspace;

use crate::app::messages::Status;

/// The unsaved-changes check of every quit that would end the editor (`:q`, `C-w q`, `:wq`/`:x`, `ZZ`/`ZQ`
/// on the last window, `:qa`): refuse with E37 when the focused buffer is modified, or E162 when another
/// (hidden) one is. `abandon_focused` (`:q!`, `ZQ`) skips only the focused buffer's check; a modified
/// hidden buffer still refuses, as in nvim. Returns whether to quit.
pub(crate) fn may_quit(ws: &Workspace, status: &mut Status, abandon_focused: bool) -> bool {
    let focused = ws.focused_buffer();
    if !abandon_focused && ws.focused().doc.is_modified() {
        status.error("E37: No write since last change (add ! to override)");
        false
    } else if let Some(b) = ws
        .buffers()
        .into_iter()
        .find(|b| b.modified && b.id != focused)
    {
        status.error(no_write_since_last_change(&b.name));
        false
    } else {
        true
    }
}

/// Neovim's E162 refusal: buffer `name` (not the focused one, or any one for a whole-editor quit) has
/// unsaved changes.
pub(super) fn no_write_since_last_change(name: &str) -> String {
    format!("E162: No write since last change for buffer \"{name}\"")
}

/// The process exit status a `:cquit` asked for; 0 otherwise. Read by the CLI once the session returns.
static EXIT_STATUS: AtomicU8 = AtomicU8::new(0);

/// The exit status the session should end with (`:cq [N]` sets it; a normal quit leaves it 0).
pub(crate) fn exit_status() -> u8 {
    EXIT_STATUS.load(Ordering::Relaxed)
}

/// Record the exit status `:cq [N]` asked for.
pub(crate) fn set_exit_status(code: u8) {
    EXIT_STATUS.store(code, Ordering::Relaxed);
}

/// `:q` (Vim `:quit`): with other windows open, close the focused one — its buffer stays loaded and
/// hidden even when modified (#736; nvim's 'hidden' is on by default). On the last window, end the editor
/// through [`may_quit`]. `force` (`:q!`, `ZQ`) lets the FOCUSED buffer's changes go; a modified hidden
/// buffer still refuses, as in nvim, so `:q!` can never silently drop work the user cannot see.
pub(crate) fn quit_window(ws: &mut Workspace, status: &mut Status, quit: &mut bool, force: bool) {
    if ws.close_focused() {
        return;
    }
    *quit = may_quit(ws, status, force);
}

#[cfg(test)]
mod tests;
