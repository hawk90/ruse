//! Registers and the OS clipboard: the editor-global register store the workspace lends into the
//! focused View, the `"+`/`"*` provider, and the register snapshots the frontend reads.

use crate::register::RegKind;
use crate::{Clipboard, EditorState, RegisterStore, Workspace};

impl Workspace {
    /// The editor-global register store (read-only).
    #[must_use]
    pub fn registers(&self) -> &RegisterStore {
        &self.registers
    }

    /// The `"+`/`"*` value a clipboard pull wrote since the last call, if a pull changed the slot: what
    /// another application copied, read as external input. The frontend records it in the trace ahead of
    /// the command that read it (an `ex_register` record, RFC-0020 R6), so a replay, which has no clipboard,
    /// puts the same text.
    pub fn take_clipboard_read(&mut self) -> Option<crate::register::Register> {
        self.clipboard_read.take()
    }

    pub(super) fn note_clipboard_read(&mut self, pulled: Option<crate::register::Register>) {
        if pulled.is_some() {
            self.clipboard_read = pulled;
        }
    }

    /// Install the OS-clipboard provider backing `"+`/`"*` (`:help quoteplus`). The frontend calls this once
    /// at startup with a real shell-out provider; tests inject an in-memory double. Without it the workspace
    /// keeps the [`NoClipboard`] default, so `"+p` is a graceful no-op and `"+y` silently drops.
    pub fn set_clipboard(&mut self, clipboard: Box<dyn Clipboard>) {
        self.clipboard = clipboard;
    }

    /// Store raw macro bytes into a named register of the FOCUSED view (D-055; the swap-trick). Shares the
    /// a-z slots with yank/paste, so a recorded macro pastes as text and yanked text runs as a macro.
    pub fn set_register_raw(&mut self, name: Option<char>, bytes: Vec<u8>) {
        self.with_focused(|st| st.set_register_raw(name, bytes));
    }

    /// Sync the four read-only special registers `"/ ": ". "%` of the FOCUSED view from frontend state
    /// (the swap-trick). The frontend calls this before dispatching a register-reading command so a
    /// following `"/p` / `C-r :` / `".p` / `"%p` resolves to the live search pattern, Ex line, inserted
    /// text, or file name. `None` clears a slot (no search yet, or an unnamed buffer for `"%`).
    pub fn set_special_registers(
        &mut self,
        search: Option<String>,
        last_ex: Option<String>,
        inserted: Option<String>,
        file: Option<String>,
    ) {
        self.with_focused(|st| st.set_special_registers(search, last_ex, inserted, file));
    }

    /// Sync `"#`, the alternate file name (`:help quote#`); `None` when there is no alternate file.
    pub fn set_alternate_register(&mut self, name: Option<String>) {
        self.registers.set_alternate(name);
    }

    /// The raw bytes of a named register of the FOCUSED view (D-055 macro replay). Empty
    /// when the register is unset.
    #[must_use]
    pub fn register_bytes(&self, name: Option<char>) -> Vec<u8> {
        self.registers.get(name).text().to_vec()
    }

    /// A snapshot of the FOCUSED view's NON-EMPTY registers as `(name, kind, bytes)` — the unnamed slot
    /// (`"`), the yank register (`0`), and the named `a`-`z` — for `:registers`. Order: `"`,
    /// `0`, a..z. The [`RegKind`] drives the frontend's Type column (`c`/`l`/`b`, per Vim's `:reg` layout).
    #[must_use]
    pub fn register_snapshot(&self) -> Vec<(char, RegKind, Vec<u8>)> {
        let regs = &self.registers;
        let mut out = Vec::new();
        // Vim `:reg` order: unnamed, the yank register "0, the numbered delete-ring "1–"9, the small-delete
        // register "-, then the named slots "a–"z. Empty slots are omitted.
        for (name, r) in std::iter::once(('"', regs.get(None)))
            .chain(std::iter::once(('0', regs.yank0())))
            .chain(('1'..='9').map(|c| (c, regs.get(Some(c)))))
            .chain(std::iter::once(('-', regs.get(Some('-')))))
            .chain(('a'..='z').map(|c| (c, regs.get(Some(c)))))
        {
            if r.is_set() {
                out.push((name, r.kind(), r.text().to_vec()));
            }
        }
        out
    }
}

/// Pull the OS clipboard into the `"+`/`"*` mirror slot of `st`, returning the slot's new value when the pull
/// changed it (what the trace records, RFC-0020 R6), `None` when the provider had nothing or the same bytes.
pub(super) fn pull_clipboard(
    st: &mut EditorState,
    clipboard: &mut dyn Clipboard,
) -> Option<crate::register::Register> {
    let text = clipboard.get()?.into_bytes();
    if st.registers().clipboard().text() == text.as_slice() {
        return None;
    }
    st.sync_clipboard_in(text);
    Some(st.registers().clipboard().clone())
}
