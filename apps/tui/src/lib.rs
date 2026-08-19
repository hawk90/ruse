//! Library face of the `ruse-tui` crate — it owns the **entire** terminal frontend.
//!
//! The `ruse` binary (`main.rs`) is a one-line shim over [`run`]; all the real code (input engine, UI,
//! terminal lifecycle, persistence, recovery, the event loop) lives here. Keeping it in the library —
//! rather than under `main.rs` — means the parity harnesses (`tests/parity_compare.rs`,
//! `tests/emacs_parity_compare.rs`) and the criterion benches drive the *same* compiled modules the binary
//! runs, instead of a separately re-compiled copy. The load-bearing flow is unchanged:
//! `KeyEvent → input::InputEngine → ruse_core::Command → plan/commit → Effect → this frontend performs it`.

// D-041: diagnostics go through `tracing`, never the terminal. The only sanctioned stdout/stderr is the
// headless CLI (`app::cli`), which carries a scoped `allow` on each such function. A non-test `.unwrap()`
// is an unjustified panic (use `.expect("<why>")` or a `Result`); tests exempt via clippy.toml.
#![deny(
    clippy::print_stdout,
    clippy::print_stderr,
    clippy::unwrap_used,
    clippy::disallowed_methods
)]

// Public surface: the binary entry (`run`) plus the modules the integration tests and benches import
// directly. Everything else is crate-internal.
pub mod caps;
pub mod highlight;
pub mod input;
pub mod line_index;
pub mod persist;
pub mod screen;

pub(crate) mod app;
pub(crate) mod health;
pub(crate) mod indent;
pub(crate) mod log;
// F-014 built-in LSP client + normalized diagnostics model (slice 1) — cross-platform (stdio pipes).
pub(crate) mod lsp;
// F-011 PTY-backed terminal buffer (slice 1) — unix-only (forkpty via `libc`); other targets stub `:terminal`.
#[cfg(unix)]
pub(crate) mod pty;
pub(crate) mod recover;
#[cfg(unix)]
pub(crate) mod term_buffer;
#[cfg(unix)]
pub(crate) mod term_grid;
pub(crate) mod terminal;
pub(crate) mod ui;
pub(crate) mod viewport;

pub use app::cli::run;
