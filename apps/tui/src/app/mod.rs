//! The application layer: the event-loop hub (`session::run`) and the frontend's command/ex dispatch
//! (`dispatch`). `main.rs` only bootstraps and hands off to `session::run`.

pub(crate) mod dispatch;
pub(crate) mod session;
