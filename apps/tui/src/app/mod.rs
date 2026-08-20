//! The application layer: the CLI entry point (`cli::run`), the event-loop hub (`session::run`), and
//! the frontend's command/ex dispatch (`dispatch`). `main.rs` is a one-line shim over `cli::run`.

pub(crate) mod cli;
pub(crate) mod dispatch;
pub(crate) mod lsp_coordinator;
pub(crate) mod session;
