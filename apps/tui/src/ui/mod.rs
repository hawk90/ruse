//! The frontend view layer: pure renderers over `screen::Screen` (`render`), window layout geometry
//! (`layout`), and the modal overlays — command palette, line picker, and prompts.

pub(crate) mod layout;
pub(crate) mod line_picker;
pub(crate) mod palette;
pub(crate) mod prompts;
pub(crate) mod render;
