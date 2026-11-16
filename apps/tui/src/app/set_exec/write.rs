//! The write family: the session's write preferences, the backup options and the swap-file options.

use ruse_core::{DocumentId, OptionLevel};

use super::{journal_dir, OptionHost};
use crate::input::set_cmd::{Opt, OptVal};

impl OptionHost<'_> {
    /// The value of a write-family option (`buf`'s own `'swapfile'`).
    pub(super) fn get_write(&self, opt: Opt, buf: DocumentId) -> OptVal {
        match opt {
            Opt::FixEol => OptVal::Bool(self.files.write.fixeol),
            Opt::Backup => OptVal::Bool(self.files.write.backup),
            Opt::WriteBackup => OptVal::Bool(self.files.write.writebackup),
            Opt::Write => OptVal::Bool(self.files.write.write),
            Opt::WriteAny => OptVal::Bool(self.files.write.writeany),
            Opt::Fsync => OptVal::Bool(self.files.write.fsync),
            Opt::BackupDir => OptVal::Text(self.files.write.backupdir().into()),
            Opt::BackupSkip => OptVal::Text(self.files.write.backupskip().into()),
            Opt::BackupExt => OptVal::Text(self.files.write.backupext.clone().into()),
            Opt::BackupCopy => OptVal::Text(self.files.write.backupcopy.clone().into()),
            Opt::PatchMode => OptVal::Text(self.files.write.patchmode.clone().into()),
            Opt::SwapFile => OptVal::Bool(
                self.files
                    .get(&buf)
                    .map_or(self.files.write.swapfile, |bf| bf.swapfile),
            ),
            Opt::UpdateCount => {
                OptVal::Num(i64::try_from(self.files.write.updatecount).unwrap_or(i64::MAX))
            }
            Opt::Directory => OptVal::Text(journal_dir().into()),
            Opt::FileFormats => OptVal::Text(self.files.write.fileformats.clone().into()),
            _ => unreachable!(),
        }
    }

    /// The session's write preferences and the swap-file options.
    pub(super) fn put_write(
        &mut self,
        opt: Opt,
        level: OptionLevel,
        val: OptVal,
        on: bool,
        text: String,
        buf: DocumentId,
    ) {
        match (opt, val) {
            (Opt::FixEol, _) => self.files.write.fixeol = on,
            (Opt::FileFormats, _) => self.files.write.fileformats = text,
            // `:setlocal noswf` is the buffer's; `:set` also sets the default of buffers opened later.
            (Opt::SwapFile, _) => {
                if let Some(bf) = self.files.get_mut(&buf) {
                    bf.swapfile = on;
                }
                if level != OptionLevel::Local {
                    self.files.write.swapfile = on;
                }
            }
            (Opt::UpdateCount, OptVal::Num(n)) => {
                self.files.write.updatecount = usize::try_from(n).unwrap_or(0);
            }
            (Opt::Backup, _) => self.files.write.backup = on,
            (Opt::WriteBackup, _) => self.files.write.writebackup = on,
            (Opt::Write, _) => self.files.write.write = on,
            (Opt::WriteAny, _) => self.files.write.writeany = on,
            (Opt::Fsync, _) => self.files.write.fsync = on,
            (Opt::BackupDir, _) => self.files.write.backupdir = Some(text),
            (Opt::BackupSkip, _) => self.files.write.backupskip = Some(text),
            (Opt::BackupExt, _) => self.files.write.backupext = text,
            (Opt::BackupCopy, _) => self.files.write.backupcopy = text,
            (Opt::PatchMode, _) => self.files.write.patchmode = text,
            _ => {}
        }
    }
}
