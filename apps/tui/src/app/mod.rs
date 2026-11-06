//! The application layer: the CLI entry point (`cli::run`), the event-loop hub (`session::run`), and
//! the frontend's command/ex dispatch (`dispatch`). `main.rs` is a one-line shim over `cli::run`.

pub(crate) mod cli;
pub(crate) mod dispatch;
pub(crate) mod ex_more;
pub(crate) mod ex_print;
pub(crate) mod ex_range;
pub(crate) mod file_cmds;
pub(crate) mod fold_cmds;
pub(crate) mod indent_keys;
pub(crate) mod lsp_coordinator;
pub(crate) mod session;
pub(crate) mod set_exec;
pub(crate) mod trace_check;
