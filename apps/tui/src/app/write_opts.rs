//! The session's write policy (#619): the Vim options `:set` reads and writes (`'backup'`, `'writebackup'`,
//! `'backupdir'`, `'backupext'`, `'backupcopy'`, `'backupskip'`, `'patchmode'`, `'write'`, `'writeany'`,
//! `'fsync'`, `'fixeol'`), the Emacs variables of the Emacs profile (read from the `emacs.*` config keys),
//! and how both become one [`backup::Policy`] and one [`atomic::Opts`] for every write. The per-file state
//! (`'endofline'`, `'endoffile'`, `'bomb'`) lives in each buffer's [`FileFormat`].
//!
//! Each profile keeps its own editor's rules and defaults: the Vim and Native profiles behave as nvim
//! v0.12.6 (no kept backup, a write backup for an in-place write), the Emacs profile as Emacs 31.1
//! (`make-backup-files` on: the first save of a visit keeps `f~`; `require-final-newline` from
//! `mode-require-final-newline`, t). Emacs's `file-precious-flag`, `break-hardlink-on-save` and
//! `file-preserve-symlinks-on-save` select the same write methods as `'backupcopy'`.

use std::collections::HashMap;
use std::path::PathBuf;

use ruse_core::DocumentId;

use crate::persist::atomic::{self, Copy};
use crate::persist::backup::{self, Copying, DeleteOld, VersionControl};
use crate::persist::encoding::FileFormat;

/// Emacs's `require-final-newline` / `mode-require-final-newline`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FinalNewline {
    /// `t`: a save adds a missing final newline.
    Add,
    /// `nil`: never.
    Never,
    /// `visit`: Emacs adds it when the file is visited. ruse never edits a buffer on open, so this adds
    /// nothing (recorded divergence).
    Visit,
    /// `visit-save`: on visiting (not done, as `visit`) and on saving.
    VisitSave,
    /// Any other value: Emacs asks on each save. ruse does not prompt in the middle of a save; it adds
    /// nothing and the write report shows `[noeol]` (recorded divergence).
    Ask,
}

impl FinalNewline {
    pub(crate) fn parse(s: &str) -> FinalNewline {
        match s {
            "t" => FinalNewline::Add,
            "nil" => FinalNewline::Never,
            "visit" => FinalNewline::Visit,
            "visit-save" => FinalNewline::VisitSave,
            _ => FinalNewline::Ask,
        }
    }

    fn on_save(self) -> bool {
        matches!(self, FinalNewline::Add | FinalNewline::VisitSave)
    }
}

/// The Emacs file variables (config keys `emacs.*`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct EmacsFiles {
    pub make_backup_files: bool,
    pub backup_directory_alist: Vec<(String, String)>,
    pub version_control: VersionControl,
    pub kept_new_versions: usize,
    pub kept_old_versions: usize,
    pub delete_old_versions: DeleteOld,
    pub copying: Copying,
    pub file_precious_flag: bool,
    pub break_hardlink_on_save: bool,
    pub file_preserve_symlinks_on_save: bool,
    /// `require-final-newline`, when a config sets it; else `mode-require-final-newline` applies, as every
    /// buffer visiting a file is in a text or programming mode.
    pub require_final_newline: Option<FinalNewline>,
    pub mode_require_final_newline: FinalNewline,
    pub temporary_file_directory: String,
    pub small_temporary_file_directory: String,
    /// The buffer and quit variables (#619, `app/session/emacs_files.rs`).
    pub view_read_only: bool,
    /// `confirm-kill-emacs`: `nil`, `yes-or-no-p` or `y-or-n-p`.
    pub confirm_kill_emacs: String,
    pub confirm_kill_processes: bool,
    /// `buffer-offer-save`: `nil`, `t` or `always`.
    pub buffer_offer_save: String,
    pub query_about_changed_file: bool,
    pub revert_without_query: Vec<String>,
    pub revert_buffer_quick_short_answers: bool,
    /// `save-some-buffers-default-predicate` is `save-some-buffers-root`.
    pub save_some_buffers_default_predicate_root: bool,
}

impl Default for EmacsFiles {
    fn default() -> EmacsFiles {
        EmacsFiles {
            make_backup_files: true,
            backup_directory_alist: Vec::new(),
            version_control: VersionControl::Existing,
            kept_new_versions: 2,
            kept_old_versions: 2,
            delete_old_versions: DeleteOld::Ask,
            copying: Copying::EMACS,
            file_precious_flag: false,
            break_hardlink_on_save: false,
            file_preserve_symlinks_on_save: false,
            require_final_newline: None,
            mode_require_final_newline: FinalNewline::Add,
            temporary_file_directory: String::new(),
            small_temporary_file_directory: String::new(),
            view_read_only: false,
            confirm_kill_emacs: "nil".into(),
            confirm_kill_processes: true,
            buffer_offer_save: "nil".into(),
            query_about_changed_file: true,
            revert_without_query: Vec::new(),
            revert_buffer_quick_short_answers: false,
            save_some_buffers_default_predicate_root: false,
        }
    }
}

impl EmacsFiles {
    /// `temporary-file-directory` (empty: `$TMPDIR`, else `/tmp`) and `small-temporary-file-directory`.
    fn temp_dirs(&self) -> Vec<PathBuf> {
        let mut out = Vec::new();
        let tmp = if self.temporary_file_directory.is_empty() {
            std::env::temp_dir()
        } else {
            PathBuf::from(backup::expand(&self.temporary_file_directory))
        };
        out.push(tmp);
        if !self.small_temporary_file_directory.is_empty() {
            out.push(PathBuf::from(backup::expand(
                &self.small_temporary_file_directory,
            )));
        }
        out
    }

    /// Read the `emacs.*` config keys over the Emacs defaults.
    pub(crate) fn from_config(cfg: &crate::config::Config) -> EmacsFiles {
        use crate::config::Value;
        let mut e = EmacsFiles::default();
        let get = |k: &str| cfg.get(&format!("emacs.{k}")).map(|v| &v.value);
        let flag = |k: &str, d: bool| match get(k) {
            Some(Value::Bool(b)) => *b,
            _ => d,
        };
        let count = |k: &str, d: usize| match get(k) {
            Some(Value::Int(n)) => usize::try_from(*n).unwrap_or(0),
            _ => d,
        };
        let text = |k: &str| match get(k) {
            Some(Value::Str(s)) => Some(s.clone()),
            _ => None,
        };
        e.make_backup_files = flag("make_backup_files", e.make_backup_files);
        if let Some(Value::Map(m)) = get("backup_directory_alist") {
            e.backup_directory_alist = m
                .iter()
                .filter_map(|(re, d)| match d {
                    Value::Str(d) => Some((re.clone(), d.clone())),
                    _ => None,
                })
                .collect();
        }
        if let Some(v) = text("version_control") {
            e.version_control = match v.as_str() {
                "t" => VersionControl::Always,
                "never" => VersionControl::Never,
                _ => VersionControl::Existing,
            };
        }
        e.kept_new_versions = count("kept_new_versions", e.kept_new_versions);
        e.kept_old_versions = count("kept_old_versions", e.kept_old_versions);
        if let Some(v) = text("delete_old_versions") {
            e.delete_old_versions = match v.as_str() {
                "t" => DeleteOld::Yes,
                "nil" => DeleteOld::Ask,
                _ => DeleteOld::Never,
            };
        }
        e.copying.always = flag("backup_by_copying", e.copying.always);
        e.copying.when_linked = flag("backup_by_copying_when_linked", e.copying.when_linked);
        e.copying.when_mismatch = flag("backup_by_copying_when_mismatch", e.copying.when_mismatch);
        if let Some(Value::Int(n)) = get("backup_by_copying_when_privileged_mismatch") {
            e.copying.when_privileged_mismatch = u32::try_from(*n).ok();
        }
        e.file_precious_flag = flag("file_precious_flag", e.file_precious_flag);
        e.break_hardlink_on_save = flag("break_hardlink_on_save", e.break_hardlink_on_save);
        e.file_preserve_symlinks_on_save = flag(
            "file_preserve_symlinks_on_save",
            e.file_preserve_symlinks_on_save,
        );
        e.require_final_newline = text("require_final_newline").map(|s| FinalNewline::parse(&s));
        if let Some(v) = text("mode_require_final_newline") {
            e.mode_require_final_newline = FinalNewline::parse(&v);
        }
        e.temporary_file_directory = text("temporary_file_directory").unwrap_or_default();
        e.small_temporary_file_directory =
            text("small_temporary_file_directory").unwrap_or_default();
        e.view_read_only = flag("view_read_only", e.view_read_only);
        if let Some(v) = text("confirm_kill_emacs") {
            e.confirm_kill_emacs = v;
        }
        e.confirm_kill_processes = flag("confirm_kill_processes", e.confirm_kill_processes);
        if let Some(v) = text("buffer_offer_save") {
            e.buffer_offer_save = v;
        }
        e.query_about_changed_file = flag("query_about_changed_file", e.query_about_changed_file);
        if let Some(Value::List(l)) = get("revert_without_query") {
            e.revert_without_query = l.clone();
        }
        e.revert_buffer_quick_short_answers = flag(
            "revert_buffer_quick_short_answers",
            e.revert_buffer_quick_short_answers,
        );
        e.save_some_buffers_default_predicate_root = text("save_some_buffers_default_predicate")
            .is_some_and(|v| v == "save-some-buffers-root");
        e
    }

    /// The `require-final-newline` in force for a buffer visiting a file.
    pub(crate) fn final_newline(&self) -> FinalNewline {
        self.require_final_newline
            .unwrap_or(self.mode_require_final_newline)
    }
}

/// The write options of a session.
#[derive(Clone, Debug)]
pub(crate) struct WriteOptions {
    /// `'fixeol'`: OFF in ruse (a file opened without a final newline keeps it missing).
    pub fixeol: bool,
    pub backup: bool,
    pub writebackup: bool,
    /// `None` is the default (`.,$XDG_STATE_HOME/ruse/backup//`), computed when read.
    pub backupdir: Option<String>,
    pub backupext: String,
    pub backupcopy: String,
    /// `None` is the default (the temp directories).
    pub backupskip: Option<String>,
    pub patchmode: String,
    pub write: bool,
    pub writeany: bool,
    pub fsync: bool,
    /// The Emacs profile's variables; `None` in the Vim and Native profiles.
    pub emacs: Option<EmacsFiles>,
    /// `'endofline'` / `'endoffile'` / `'bomb'` of buffers without a file, for their first write.
    pub nofile_fmt: HashMap<DocumentId, FileFormat>,
}

impl Default for WriteOptions {
    fn default() -> WriteOptions {
        WriteOptions::new(false)
    }
}

impl WriteOptions {
    /// nvim's defaults; with `emacs`, the Emacs profile's.
    pub(crate) fn new(emacs: bool) -> WriteOptions {
        WriteOptions {
            fixeol: false,
            backup: false,
            writebackup: true,
            backupdir: None,
            backupext: "~".into(),
            backupcopy: "auto".into(),
            backupskip: None,
            patchmode: String::new(),
            write: true,
            writeany: false,
            fsync: true,
            emacs: emacs.then(EmacsFiles::default),
            nofile_fmt: HashMap::new(),
        }
    }

    pub(crate) fn backupdir(&self) -> String {
        self.backupdir
            .clone()
            .unwrap_or_else(backup::default_backupdir)
    }

    pub(crate) fn backupskip(&self) -> String {
        self.backupskip
            .clone()
            .unwrap_or_else(backup::default_backupskip)
    }

    /// The backups every write makes.
    pub(crate) fn policy(&self) -> backup::Policy {
        match &self.emacs {
            Some(e) => backup::Policy {
                keep: e.make_backup_files,
                write: false,
                once_per_visit: true,
                skip: String::new(),
                patchmode: String::new(),
                rules: backup::Rules::Emacs(backup::Emacs {
                    alist: e.backup_directory_alist.clone(),
                    version_control: e.version_control,
                    kept_old: e.kept_old_versions,
                    kept_new: e.kept_new_versions,
                    delete_old: e.delete_old_versions,
                    temp_dirs: e.temp_dirs(),
                    copying: e.copying,
                }),
            },
            None => backup::Policy {
                keep: self.backup,
                write: self.writebackup,
                once_per_visit: false,
                skip: self.backupskip(),
                patchmode: self.patchmode.clone(),
                rules: backup::Rules::Vim {
                    dirs: self.backupdir(),
                    ext: self.backupext.clone(),
                },
            },
        }
    }

    /// The write method and syncs of every write, with `policy`'s backups.
    pub(crate) fn opts<'a>(&self, policy: &'a backup::Policy, force: bool) -> atomic::Opts<'a> {
        let mut o = atomic::Opts {
            force,
            backup: Some(policy),
            fsync: self.fsync,
            ..atomic::Opts::new(force)
        };
        match &self.emacs {
            Some(e) => {
                if e.file_precious_flag {
                    o.copy = Copy::No;
                    o.break_symlink = !e.file_preserve_symlinks_on_save;
                }
                o.break_hardlink = e.break_hardlink_on_save;
            }
            None => {
                for item in self.backupcopy.split(',') {
                    match item {
                        "yes" => o.copy = Copy::Yes,
                        "no" => o.copy = Copy::No,
                        "breaksymlink" => o.break_symlink = true,
                        "breakhardlink" => o.break_hardlink = true,
                        _ => {}
                    }
                }
            }
        }
        o
    }

    /// Whether a write adds a final newline the buffer lacks: `'fixeol'` (Vim) or `require-final-newline`
    /// (Emacs), else the buffer's own `'endofline'`.
    pub(crate) fn add_final_newline(&self, fmt: FileFormat) -> bool {
        match &self.emacs {
            Some(e) => e.final_newline().on_save() || fmt.final_newline,
            None => self.fixeol || fmt.final_newline,
        }
    }

    /// Whether a write ends the file with `'endoffile'`'s CTRL-Z: nvim writes it only without `'fixeol'`.
    pub(crate) fn add_eof(&self, fmt: FileFormat) -> bool {
        fmt.eof && !(self.emacs.is_none() && self.fixeol)
    }

    /// The text a write puts in the file of a buffer read as `fmt`, and its bytes on disk.
    pub(crate) fn encode(&self, text: &[u8], fmt: FileFormat) -> (Vec<u8>, Vec<u8>) {
        let mut source = text.to_vec();
        // The newline is added before `to_disk`, so a CRLF file gets `\r\n`.
        if self.add_final_newline(fmt) && !source.is_empty() && source.last() != Some(&b'\n') {
            source.push(b'\n');
        }
        let fmt = FileFormat {
            eof: self.add_eof(fmt),
            ..fmt
        };
        let disk = fmt.to_disk(&source);
        (source, disk)
    }

    /// `E142` when `'write'` is off.
    pub(crate) fn check_write(&self) -> Result<(), &'static str> {
        if self.write {
            Ok(())
        } else {
            Err("E142: File not written: Writing is disabled by 'write' option")
        }
    }
}

/// Write `disk` to `path` under the session's write options, `force` for `:w!`.
pub(crate) fn write_file(
    w: &WriteOptions,
    path: &std::path::Path,
    disk: &[u8],
    force: bool,
) -> Result<atomic::Written, atomic::SaveError> {
    let policy = w.policy();
    atomic::write_with(path, disk, &w.opts(&policy, force))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backupcopy_values_are_checked_as_nvim_does() {
        use crate::input::set_cmd::backupcopy_valid;
        assert!(backupcopy_valid("auto"));
        assert!(backupcopy_valid("yes,breakhardlink"));
        assert!(!backupcopy_valid("foo"));
        assert!(!backupcopy_valid("yes,no"));
        assert!(!backupcopy_valid("breakhardlink"));
        assert!(!backupcopy_valid(""));
    }

    #[test]
    fn profiles_pick_their_editors_rules() {
        let vim = WriteOptions::new(false).policy();
        assert!(!vim.keep && vim.write && vim.emacs().is_none());
        let emacs = WriteOptions::new(true).policy();
        assert!(emacs.keep && emacs.once_per_visit && emacs.emacs().is_some());
    }

    #[test]
    fn final_newline_follows_fixeol_or_require_final_newline() {
        let noeol = FileFormat {
            final_newline: false,
            ..FileFormat::default()
        };
        let mut vim = WriteOptions::new(false);
        assert_eq!(vim.encode(b"a", noeol).1, b"a");
        vim.fixeol = true;
        assert_eq!(vim.encode(b"a", noeol).1, b"a\n");
        let mut emacs = WriteOptions::new(true);
        assert_eq!(
            emacs.encode(b"a", noeol).1,
            b"a\n",
            "mode-require-final-newline t"
        );
        if let Some(e) = &mut emacs.emacs {
            e.require_final_newline = Some(FinalNewline::Never);
        }
        assert_eq!(emacs.encode(b"a", noeol).1, b"a");
    }

    #[test]
    fn endoffile_is_written_without_fixeol() {
        let fmt = FileFormat {
            eof: true,
            ..FileFormat::default()
        };
        let mut w = WriteOptions::new(false);
        assert_eq!(w.encode(b"a\n", fmt).1, b"a\n\x1a");
        w.fixeol = true;
        assert_eq!(w.encode(b"a\n", fmt).1, b"a\n");
    }

    #[test]
    fn emacs_precious_and_hardlink_flags_map_onto_backupcopy() {
        let mut w = WriteOptions::new(true);
        let p = w.policy();
        let o = w.opts(&p, false);
        assert_eq!(
            (o.copy, o.break_symlink, o.break_hardlink),
            (Copy::Auto, false, false)
        );
        if let Some(e) = &mut w.emacs {
            e.file_precious_flag = true;
            e.break_hardlink_on_save = true;
        }
        let o = w.opts(&p, false);
        assert_eq!(
            (o.copy, o.break_symlink, o.break_hardlink),
            (Copy::No, true, true)
        );
        let mut v = WriteOptions::new(false);
        v.backupcopy = "yes,breaksymlink".into();
        let o = v.opts(&p, false);
        assert_eq!((o.copy, o.break_symlink), (Copy::Yes, true));
    }
}
