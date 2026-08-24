//! The frontend view layer: pure renderers over `screen::Screen` (`render`), window layout geometry
//! (`layout`), and the modal overlays — command palette, line picker, buffer picker, and prompts.

pub(crate) mod action_picker;
pub(crate) mod buffer_picker;
pub(crate) mod diag_picker;
pub(crate) mod digraph_picker;
pub(crate) mod file_picker;
pub(crate) mod layout;
pub(crate) mod line_picker;
pub(crate) mod marks_picker;
pub(crate) mod palette;
pub(crate) mod picker;
pub(crate) mod pos_picker;
pub(crate) mod prompts;
pub(crate) mod ref_picker;
pub(crate) mod register_picker;
pub(crate) mod render;
