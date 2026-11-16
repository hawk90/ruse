//! The session state a semantic command runs against (#1509).

use ruse_core::{Command, Workspace};

use crate::app::dispatch::{run_cmd, run_cmd_reported, Files};
use crate::app::messages::Status;

/// What [`run_cmd`] reads and writes: the workspace, its file buffers, the recorded command trace, the
/// status line (with the `:messages` history) and the quit flag. A session key handler borrows it whole
/// next to the other session fields it uses, and runs commands through [`Editing::run`].
pub(crate) struct Editing {
    pub(crate) ws: Workspace,
    pub(crate) files: Files,
    pub(crate) recorded: Vec<Command>,
    /// The status line's message and the `:messages` history (#1235).
    pub(crate) status: Status,
    pub(crate) quit: bool,
}

impl Editing {
    /// Run `cmd` through [`run_cmd`] with these fields.
    pub(crate) fn run(&mut self, cmd: Command) {
        run_cmd(
            cmd,
            &mut self.ws,
            &self.files,
            &mut self.recorded,
            &mut self.status,
            &mut self.quit,
        );
    }

    /// Run `cmd` through [`run_cmd_reported`] with these fields: nvim's report for a `report` threshold.
    pub(crate) fn run_reported(&mut self, cmd: Command, report: i64) {
        run_cmd_reported(
            cmd,
            &mut self.ws,
            &self.files,
            &mut self.recorded,
            &mut self.status,
            &mut self.quit,
            report,
        );
    }
}
