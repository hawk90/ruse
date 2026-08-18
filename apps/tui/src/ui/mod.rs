//! The frontend view layer: pure renderers over `screen::Screen` (`render`) and the window layout
//! geometry (`layout`). These are pure over the terminal buffer — they hold no event-loop state.

pub(crate) mod layout;
pub(crate) mod render;
