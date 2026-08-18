//! The application layer: the frontend's command/ex dispatch (`dispatch`). The event-loop hub
//! (`fn run`) still lives in main.rs for now; it is the last thing to move.

pub(crate) mod dispatch;
