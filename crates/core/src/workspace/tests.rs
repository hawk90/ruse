//! Unit tests of the workspace, one file per topic under `tests/`; the shared fixtures live here.

use super::*;
use crate::clipboard::MemClipboard;
use crate::command::Command;
use crate::motion::Motion;
use crate::motion::Motion as Mo;

fn ws() -> Workspace {
    Workspace::new(b"hello\nworld\nfoo\nbar\n".to_vec())
}

const PASTE: Command = Command::Paste {
    after: true,
    count: 1,
    move_after: false,
};

mod buffers_tests;
mod indent_tests;
mod line_cmds_tests;
mod options_tests;
mod queries_tests;
mod registers_tests;
mod view_state_tests;
mod windows_tests;
