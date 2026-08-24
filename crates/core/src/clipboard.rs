//! The system-clipboard PROVIDER seam — the abstraction behind the `"+`/`"*` registers (`:help quoteplus`).
//!
//! The register model in [`crate::register`] is pure and view-free; talking to the OS clipboard is an
//! IMPURE side effect (a subprocess, a platform API) that must not leak into the planner. So the clipboard
//! is a small injectable trait: [`Workspace`](crate::Workspace) holds a `Box<dyn Clipboard>`, defaulting to
//! [`NoClipboard`] (a no-op, so core is pure and CI stays deterministic). The frontend injects a real
//! shell-out implementation at startup; unit tests inject [`MemClipboard`], an in-memory double they can
//! also read back. This mirrors the terminal-PTY / LSP-client / remote-transport seams (interface in core,
//! live impl in `apps/tui`, mock in tests).
//!
//! Text-only, [`Option<String>`] on read: a `get` returns `None` when no clipboard tool is available or the
//! clipboard is empty (so `"+p` with nothing to paste is a graceful no-op, never a crash), and `set` is
//! best-effort (silently dropped when no tool exists). The linewise/charwise geometry a register carries is
//! reconstructed by [`RegisterStore`](crate::register::RegisterStore) from the text, since the OS clipboard
//! itself is untyped bytes.

/// An injectable OS-clipboard provider. Both methods take `&self` (interior mutability if the impl needs
/// state) so the holder can call them while mutating its other fields.
pub trait Clipboard {
    /// The clipboard's current text, or `None` when it is empty or no clipboard tool is available.
    fn get(&self) -> Option<String>;
    /// Best-effort write of `text` to the clipboard; a no-op when no clipboard tool is available.
    fn set(&self, text: &str);
}

/// The default provider: no clipboard at all. `get` yields `None`, `set` is a no-op. Keeps core pure and
/// makes `"+`/`"*` degrade gracefully anywhere a real provider was not injected (including CI).
#[derive(Clone, Copy, Debug, Default)]
pub struct NoClipboard;

impl Clipboard for NoClipboard {
    fn get(&self) -> Option<String> {
        None
    }
    fn set(&self, _text: &str) {}
}

/// An in-memory clipboard double for unit tests: a shared, cloneable handle over one `Option<String>`, so a
/// test can inject it into a [`Workspace`](crate::Workspace), drive `"+y`/`"+p`, and read the result back
/// with [`MemClipboard::contents`]. Interior mutability keeps the [`Clipboard`] `&self` contract.
#[derive(Clone, Default)]
pub struct MemClipboard(std::sync::Arc<std::sync::Mutex<Option<String>>>);

impl MemClipboard {
    /// A fresh, empty in-memory clipboard.
    #[must_use]
    pub fn new() -> MemClipboard {
        MemClipboard::default()
    }

    /// The current contents (what a `set` last wrote, or `None` if never written / preloaded).
    #[must_use]
    pub fn contents(&self) -> Option<String> {
        self.0.lock().expect("clipboard mutex").clone()
    }

    /// Seed the clipboard as if an external app had put `text` there (for a `"+p` test).
    pub fn preload(&self, text: &str) {
        *self.0.lock().expect("clipboard mutex") = Some(text.to_string());
    }
}

impl Clipboard for MemClipboard {
    fn get(&self) -> Option<String> {
        self.0.lock().expect("clipboard mutex").clone()
    }
    fn set(&self, text: &str) {
        *self.0.lock().expect("clipboard mutex") = Some(text.to_string());
    }
}
