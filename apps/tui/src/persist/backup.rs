//! Backup files (#619): the one engine behind Vim's `'backup'` / `'writebackup'` / `'backupdir'` /
//! `'backupext'` / `'backupskip'` / `'patchmode'` and Emacs's `make-backup-files` /
//! `backup-directory-alist` / `version-control` / `kept-new-versions` / `kept-old-versions` /
//! `delete-old-versions`. A [`Policy`] says which backups a write makes and where; [`make`] makes them just
//! before [`super::atomic`] replaces the file, and [`Made::finish`] removes the ones that only guard the write.
//!
//! Each profile reads its own editor's options, with that editor's defaults (nvim v0.12.6, Emacs 31.1):
//!
//! - **Vim**: a backup is made on EVERY write when `'backup'` is set and kept as `<name><backupext>` in the
//!   first `'backupdir'` entry that works (`.` is the file's directory; a trailing `//` names the backup after
//!   the whole path, `/` becoming `%`; a missing directory is created). `'writebackup'` makes the same backup
//!   for the duration of a write that overwrites the file in place and removes it after; a write by rename
//!   needs none, since the original stays whole until the rename (ruse's temp file is the write backup).
//!   `'patchmode'` keeps the oldest version as `<name><patchmode>` beside the file, once.
//! - **Emacs**: a backup is made on the FIRST save of a visit that finds the file on disk
//!   (`buffer-backed-up`), as `<name>~` or, under `version-control`, `<name>.~N~`, beside the file or in the
//!   `backup-directory-alist` directory whose regexp matches (an absolute directory names the backup after
//!   the whole path, `/` becoming `!` and `!` doubling). Excess numbered versions go when
//!   `delete-old-versions` is `t`, keeping the `kept-old-versions` oldest and `kept-new-versions` newest.
//!   Files under `temporary-file-directory` / `small-temporary-file-directory` get none
//!   (`normal-backup-enable-predicate`).
//!
//! A backup of a write by rename is a hard link to the file being replaced (exactly the old file: owner,
//! mode, times, xattrs), else a copy; a backup of an in-place write is always a copy, since the write changes
//! that inode. Backup files are created owner-only when a directory has to be made, never follow a symlink
//! at the backup name, and never replace anything but a regular file.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// Which editor's backup rules apply.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Rules {
    /// Vim: `'backupdir'` entries and `'backupext'`.
    Vim { dirs: String, ext: String },
    /// Emacs: `backup-directory-alist` and the version settings.
    Emacs(Emacs),
}

/// Emacs's `version-control`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VersionControl {
    /// `nil`: numbered when numbered backups already exist.
    Existing,
    /// `t`: always numbered.
    Always,
    /// `never`: always `<name>~`.
    Never,
}

/// Emacs's `delete-old-versions`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeleteOld {
    /// `t`: delete the excess versions.
    Yes,
    /// `nil`: Emacs asks; ruse does not prompt in the middle of a save, so it keeps them and says so.
    Ask,
    /// Anything else: never delete.
    Never,
}

/// The Emacs backup settings.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Emacs {
    /// `backup-directory-alist`: `(regexp, directory)` pairs, the first match wins.
    pub alist: Vec<(String, String)>,
    pub version_control: VersionControl,
    pub kept_old: usize,
    pub kept_new: usize,
    pub delete_old: DeleteOld,
    /// `temporary-file-directory` and `small-temporary-file-directory`: no backups under them.
    pub temp_dirs: Vec<PathBuf>,
    /// The `backup-by-copying*` settings: how the save that makes the backup writes the file.
    pub copying: Copying,
}

/// Emacs's `backup-by-copying`, `-when-linked`, `-when-mismatch` and `-when-privileged-mismatch`: when they
/// say "copy", the save that makes the backup overwrites the file in place (its inode, links and owner
/// stay); otherwise it renames a new file over it, which leaves other hard links on the old text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Copying {
    pub always: bool,
    pub when_linked: bool,
    pub when_mismatch: bool,
    /// The highest owner uid that is copied, `None` for `nil`.
    pub when_privileged_mismatch: Option<u32>,
}

impl Copying {
    /// Emacs 31.1's defaults: copy only when the owner is another user, or a uid up to 200.
    pub const EMACS: Copying = Copying {
        always: false,
        when_linked: false,
        when_mismatch: true,
        when_privileged_mismatch: Some(200),
    };

    /// Whether the save that makes the backup of the file `m` writes it in place.
    pub fn in_place(&self, m: &fs::Metadata) -> bool {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            // SAFETY: `geteuid` cannot fail.
            let me = unsafe { libc::geteuid() };
            self.always
                || (self.when_linked && m.nlink() > 1)
                || (self.when_mismatch && m.uid() != me)
                || self.when_privileged_mismatch.is_some_and(|n| m.uid() <= n)
        }
        #[cfg(not(unix))]
        {
            let _ = m;
            self.always
        }
    }
}

/// What backups a write makes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Policy {
    /// Keep the backup after a successful write (`'backup'`, `make-backup-files`).
    pub keep: bool,
    /// Guard an in-place write with a backup removed after it (`'writebackup'`).
    pub write: bool,
    /// Emacs: only the first save of a visit makes one.
    pub once_per_visit: bool,
    /// `'backupskip'`: comma-separated file patterns that get no backup (Vim).
    pub skip: String,
    /// `'patchmode'`: keep the oldest version as `<name><patchmode>` (Vim; empty is off).
    pub patchmode: String,
    pub rules: Rules,
}

impl Policy {
    /// No backups at all.
    pub fn none() -> Policy {
        Policy {
            keep: false,
            write: false,
            once_per_visit: false,
            skip: String::new(),
            patchmode: String::new(),
            rules: Rules::Vim {
                dirs: ".".into(),
                ext: "~".into(),
            },
        }
    }

    /// The Emacs rules, when they apply.
    pub fn emacs(&self) -> Option<&Emacs> {
        match &self.rules {
            Rules::Emacs(e) => Some(e),
            Rules::Vim { .. } => None,
        }
    }

    /// Whether a write of `target` (which exists) makes a backup now: `in_place` says how it will be
    /// written. `false` for a skipped file and for an Emacs visit already backed up.
    pub fn wanted(&self, target: &Path, in_place: bool) -> bool {
        let want = self.keep || (self.write && in_place) || self.patch_wanted(target);
        want && !self.skipped(target) && !(self.once_per_visit && backed_up(target))
    }

    /// Whether `'patchmode'` still has to keep the oldest version of `target`.
    fn patch_wanted(&self, target: &Path) -> bool {
        (self.keep || self.write)
            && !self.patchmode.is_empty()
            && fs::symlink_metadata(patch_path(target, &self.patchmode)).is_err()
    }

    /// Whether `target` is exempt from backups: a `'backupskip'` pattern matches it (Vim), or it is under a
    /// temporary directory (Emacs).
    pub fn skipped(&self, target: &Path) -> bool {
        let abs = absolute(target);
        match &self.rules {
            Rules::Emacs(e) => {
                let real = fs::canonicalize(target).unwrap_or_else(|_| abs.clone());
                e.temp_dirs.iter().any(|d| {
                    let d = fs::canonicalize(d).unwrap_or_else(|_| d.clone());
                    real.starts_with(&d) || abs.starts_with(&d)
                })
            }
            Rules::Vim { .. } => {
                let real = fs::canonicalize(target).unwrap_or_else(|_| abs.clone());
                split_list(&self.skip).any(|pat| {
                    let pat = expand(pat);
                    [abs.as_path(), real.as_path(), target]
                        .iter()
                        .any(|p| glob_match(pat.as_bytes(), &super::journal::path_bytes(p)))
                })
            }
        }
    }
}

/// `'patchmode'`'s file of `target`: `<target><patchmode>`, beside it.
pub fn patch_path(target: &Path, patchmode: &str) -> PathBuf {
    let mut s = target.as_os_str().to_os_string();
    s.push(patchmode);
    PathBuf::from(s)
}

/// The backups one write made, removed or kept by [`Made::finish`].
#[derive(Debug, Default)]
pub struct Made {
    /// The backup itself, if one was made.
    pub backup: Option<PathBuf>,
    /// Whether it only guards this write (`'writebackup'` without `'backup'`).
    temporary: bool,
    /// Numbered versions left over because `delete-old-versions` is `nil`.
    pub excess: usize,
}

impl Made {
    /// After the write: a write backup goes when the write succeeded and stays (and is named) when it did
    /// not, so the old text is not lost. Returns the note for a failed write.
    pub fn finish(self, written: bool) -> Option<String> {
        let path = self.backup?;
        if !self.temporary {
            return None;
        }
        if written {
            let _ = fs::remove_file(&path);
            None
        } else {
            Some(format!("the old text is kept in \"{}\"", path.display()))
        }
    }
}

/// Why no backup could be made. `Display` is nvim's message.
#[derive(Debug)]
pub struct BackupError {
    /// A copy was attempted (E509), not a rename-style backup (E510).
    pub copying: bool,
    pub err: io::Error,
}

impl std::fmt::Display for BackupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.copying {
            f.write_str("E509: Cannot create backup file (add ! to override)")
        } else {
            f.write_str("E510: Can't make backup file (add ! to override)")
        }
    }
}

/// Make the backups a write of the existing file `target` needs, before it is replaced: the backup
/// (`policy.keep`, or `policy.write` for an `in_place` write) and the `'patchmode'` original. `in_place`
/// writes need a copy; the others get a hard link where the filesystem allows one.
pub fn make(policy: &Policy, target: &Path, in_place: bool) -> Result<Made, BackupError> {
    let mut made = Made::default();
    if !policy.wanted(target, in_place) {
        return Ok(made);
    }
    let fail = |err| BackupError {
        copying: in_place,
        err,
    };
    if policy.patch_wanted(target) {
        // Best effort, as nvim's (a failure there is only E205 after the write): the write goes on.
        if let Err(e) = preserve(target, &patch_path(target, &policy.patchmode), in_place) {
            crate::log::backup_failed(target, &e);
        }
    }
    if policy.keep || (policy.write && in_place) {
        // The first place that takes the backup wins (`'backupdir'` is a list).
        let (places, excess) = place(policy, target).map_err(fail)?;
        let mut last = None;
        let mut path = None;
        for p in places {
            match preserve(target, &p, in_place) {
                Ok(()) => {
                    path = Some(p);
                    break;
                }
                Err(e) => last = Some(e),
            }
        }
        let Some(path) = path else {
            return Err(fail(
                last.unwrap_or_else(|| io::Error::other("'backupdir' is empty")),
            ));
        };
        if policy.once_per_visit {
            mark_backed_up(target);
        }
        made.temporary = !policy.keep;
        made.backup = Some(path);
        made.excess = excess;
    }
    Ok(made)
}

/// `'patchmode'` for a file that did not exist: Vim leaves an empty `<name><patchmode>`.
pub fn patch_new(policy: &Policy, target: &Path) {
    if (policy.keep || policy.write) && !policy.patchmode.is_empty() && !policy.skipped(target) {
        let p = patch_path(target, &policy.patchmode);
        let _ = fs::OpenOptions::new().write(true).create_new(true).open(p);
    }
}

/// Put the current text of `src` at `dst`: a hard link (unless `copy`), else a copy keeping its mode. An
/// existing regular file at `dst` (the previous backup) is replaced; anything else there is an error.
fn preserve(src: &Path, dst: &Path, copy: bool) -> io::Result<()> {
    match fs::symlink_metadata(dst) {
        Ok(m) if m.file_type().is_file() => fs::remove_file(dst)?,
        Ok(_) => {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "not a regular file",
            ))
        }
        Err(_) => {}
    }
    if !copy && fs::hard_link(src, dst).is_ok() {
        return Ok(());
    }
    copy_file(src, dst)
}

/// Copy `src` to a NEW file `dst` (never through a symlink planted there), with `src`'s mode, and sync it.
fn copy_file(src: &Path, dst: &Path) -> io::Result<()> {
    let meta = fs::metadata(src)?;
    let mut opts = fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
    let mut out = opts.open(dst)?;
    let copied = (|| {
        let mut input = fs::File::open(src)?;
        io::copy(&mut input, &mut out)?;
        out.set_permissions(meta.permissions())?;
        out.sync_all()
    })();
    if copied.is_err() {
        let _ = fs::remove_file(dst);
    }
    copied
}

/// Where the backup of `target` may go, in order, and how many numbered versions were left over.
fn place(policy: &Policy, target: &Path) -> io::Result<(Vec<PathBuf>, usize)> {
    match &policy.rules {
        Rules::Vim { dirs, ext } => {
            let mut last = None;
            let mut out = Vec::new();
            for entry in split_list(dirs) {
                match vim_dir(entry, target) {
                    Ok((dir, mut name)) => {
                        name.push(ext);
                        out.push(dir.join(name));
                    }
                    Err(e) => last = Some(e),
                }
            }
            match (out.is_empty(), last) {
                (true, Some(e)) => Err(e),
                _ => Ok((out, 0)),
            }
        }
        Rules::Emacs(e) => emacs_place(e, target).map(|(p, n)| (vec![p], n)),
    }
}

/// One `'backupdir'` entry for `target`: the directory (created when missing) and the backup's base name.
fn vim_dir(entry: &str, target: &Path) -> io::Result<(PathBuf, std::ffi::OsString)> {
    let file_dir = target
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let mangle = entry.len() > 2 && entry.ends_with("//");
    let entry = expand(entry.strip_suffix("//").unwrap_or(entry));
    let dir = if entry == "." {
        file_dir.to_path_buf()
    } else if let Some(rest) = entry.strip_prefix("./") {
        file_dir.join(rest)
    } else {
        PathBuf::from(entry)
    };
    ensure_dir(&dir)?;
    let name = if mangle {
        mangled(target, b'%', None)
    } else {
        target.file_name().unwrap_or_default().to_os_string()
    };
    Ok((dir, name))
}

/// The Emacs backup name of `target`, and how many excess versions were kept (`delete-old-versions` nil).
fn emacs_place(e: &Emacs, target: &Path) -> io::Result<(PathBuf, usize)> {
    let abs = absolute(target);
    let file_dir = abs.parent().unwrap_or(Path::new("/")).to_path_buf();
    let shown = abs.to_string_lossy().into_owned();
    let hit = e.alist.iter().find(|(re, _)| {
        ruse_core::pattern::Regex::compile(re, ruse_core::pattern::Options::default())
            .is_ok_and(|r| r.find_at(&shown, ruse_core::BytePos(0)).is_some())
    });
    let (dir, base) = match hit {
        Some((_, d)) => {
            let d = PathBuf::from(expand(d));
            if d.is_absolute() {
                (d, mangled(&abs, b'!', Some(b'!')))
            } else {
                (file_dir.join(d), abs.file_name().unwrap_or_default().into())
            }
        }
        None => (file_dir, abs.file_name().unwrap_or_default().into()),
    };
    ensure_dir(&dir)?;
    let versions = numbered(&dir, &base);
    let numbered_now = match e.version_control {
        VersionControl::Always => true,
        VersionControl::Existing => !versions.is_empty(),
        VersionControl::Never => false,
    };
    let mut name = base.clone();
    if !numbered_now {
        name.push("~");
        return Ok((dir.join(name), 0));
    }
    let next = versions.last().copied().unwrap_or(0) + 1;
    name.push(format!(".~{next}~"));
    // Emacs's `find-backup-file-name`: the new version counts as one of the kept new ones.
    let excess = (versions.len() + 1).saturating_sub(e.kept_old + e.kept_new);
    let mut kept = 0;
    if excess > 0 {
        let doomed = versions.iter().skip(e.kept_old).take(excess);
        match e.delete_old {
            DeleteOld::Yes => {
                for n in doomed {
                    let mut old = base.clone();
                    old.push(format!(".~{n}~"));
                    let p = dir.join(old);
                    if fs::symlink_metadata(&p).is_ok_and(|m| m.file_type().is_file()) {
                        let _ = fs::remove_file(p);
                    }
                }
            }
            DeleteOld::Ask => kept = excess,
            DeleteOld::Never => {}
        }
    }
    Ok((dir.join(name), kept))
}

/// The version numbers of the `<base>.~N~` files in `dir`, ascending.
fn numbered(dir: &Path, base: &std::ffi::OsStr) -> Vec<u64> {
    let base = super::journal::path_bytes(Path::new(base));
    let mut out: Vec<u64> = fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let name = super::journal::path_bytes(Path::new(&e.file_name()));
            let rest = name.strip_prefix(base.as_slice())?.strip_prefix(b".~")?;
            let digits = rest.strip_suffix(b"~")?;
            if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
                return None;
            }
            std::str::from_utf8(digits).ok()?.parse().ok()
        })
        .collect();
    out.sort_unstable();
    out
}

/// `target`'s whole absolute path as one file name: each `/` becomes `sep`, and an existing `sep` doubles
/// when `double` (Emacs's `!!`; Vim's `%` names are not escaped).
fn mangled(target: &Path, sep: u8, double: Option<u8>) -> std::ffi::OsString {
    let abs = absolute(target);
    let mut out = Vec::new();
    for &b in super::journal::path_bytes(&abs).iter() {
        if b == b'/' {
            out.push(sep);
        } else if double == Some(b) {
            out.extend([b, b]);
        } else {
            out.push(b);
        }
    }
    bytes_os(out)
}

#[cfg(unix)]
fn bytes_os(b: Vec<u8>) -> std::ffi::OsString {
    use std::os::unix::ffi::OsStringExt;
    std::ffi::OsString::from_vec(b)
}

#[cfg(not(unix))]
fn bytes_os(b: Vec<u8>) -> std::ffi::OsString {
    String::from_utf8_lossy(&b).into_owned().into()
}

/// Make `dir` (and its parents) when missing, owner-only.
fn ensure_dir(dir: &Path) -> io::Result<()> {
    if dir.is_dir() {
        return Ok(());
    }
    let mut b = fs::DirBuilder::new();
    b.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut b, 0o700);
    b.create(dir)
}

fn absolute(p: &Path) -> PathBuf {
    std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf())
}

/// The items of a comma-separated option value, empty ones dropped.
fn split_list(s: &str) -> impl Iterator<Item = &str> {
    s.split(',').map(str::trim).filter(|x| !x.is_empty())
}

/// `~/` and `$NAME` / `${NAME}` expanded from the environment; an unset variable expands to nothing.
pub fn expand(s: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    if let Some(r) = rest.strip_prefix("~/") {
        if let Some(h) = std::env::var_os("HOME") {
            out.push_str(&h.to_string_lossy());
            out.push('/');
            rest = r;
        }
    }
    let mut chars = rest.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        if c != '$' {
            out.push(c);
            continue;
        }
        let tail = &rest[i + 1..];
        let (name, used) = if let Some(t) = tail.strip_prefix('{') {
            match t.find('}') {
                Some(end) => (&t[..end], end + 2),
                None => ("", 0),
            }
        } else {
            let end = tail
                .find(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '_'))
                .unwrap_or(tail.len());
            (&tail[..end], end)
        };
        if name.is_empty() {
            out.push('$');
            continue;
        }
        if let Some(v) = std::env::var_os(name) {
            out.push_str(&v.to_string_lossy());
        }
        for _ in 0..used {
            chars.next();
        }
    }
    out
}

/// A Vim file pattern (`*` any run, `?` one byte, `[...]` a class, `\` escapes) against a whole path.
pub fn glob_match(pat: &[u8], s: &[u8]) -> bool {
    let (mut p, mut i) = (0, 0);
    let (mut star, mut mark) = (None, 0);
    while i < s.len() {
        if p < pat.len() {
            match pat[p] {
                b'*' => {
                    star = Some(p);
                    mark = i;
                    p += 1;
                    continue;
                }
                b'?' => {
                    p += 1;
                    i += 1;
                    continue;
                }
                b'[' => {
                    if let Some((hit, len)) = class(&pat[p..], s[i]) {
                        if hit {
                            p += len;
                            i += 1;
                            continue;
                        }
                    } else if s[i] == b'[' {
                        p += 1;
                        i += 1;
                        continue;
                    }
                }
                b'\\' if p + 1 < pat.len() && pat[p + 1] == s[i] => {
                    p += 2;
                    i += 1;
                    continue;
                }
                c if c == s[i] => {
                    p += 1;
                    i += 1;
                    continue;
                }
                _ => {}
            }
        }
        match star {
            Some(sp) => {
                p = sp + 1;
                mark += 1;
                i = mark;
            }
            None => return false,
        }
    }
    pat[p..].iter().all(|&c| c == b'*')
}

/// A `[...]` class at the start of `pat` against `c`: whether it matches and the class's length.
fn class(pat: &[u8], c: u8) -> Option<(bool, usize)> {
    let mut i = 1;
    let negate = matches!(pat.get(i), Some(b'!' | b'^'));
    if negate {
        i += 1;
    }
    let start = i;
    let mut hit = false;
    while i < pat.len() {
        if pat[i] == b']' && i > start {
            return Some((hit != negate, i + 1));
        }
        if i + 2 < pat.len() && pat[i + 1] == b'-' && pat[i + 2] != b']' {
            hit |= (pat[i]..=pat[i + 2]).contains(&c);
            i += 3;
        } else {
            hit |= pat[i] == c;
            i += 1;
        }
    }
    None
}

/// nvim's `'backupskip'` default: `/tmp/*` (`/private/tmp/*` on macOS) and the `$TMPDIR`, `$TMP` and
/// `$TEMP` directories, each once.
pub fn default_backupskip() -> String {
    let mut dirs: Vec<String> = Vec::new();
    let tmp = if cfg!(target_os = "macos") {
        "/private/tmp"
    } else {
        "/tmp"
    };
    let mut push = |d: String| {
        let d = d.trim_end_matches('/').to_string();
        if !d.is_empty() && !dirs.contains(&d) {
            dirs.push(d);
        }
    };
    push(tmp.to_string());
    for v in ["TMPDIR", "TMP", "TEMP"] {
        if let Some(d) = std::env::var_os(v) {
            push(d.to_string_lossy().into_owned());
        }
    }
    dirs.iter()
        .map(|d| format!("{d}/*"))
        .collect::<Vec<_>>()
        .join(",")
}

/// nvim's `'backupdir'` default, with ruse's state directory: `.,$XDG_STATE_HOME/ruse/backup//`.
pub fn default_backupdir() -> String {
    match super::journal::Journals::from_env() {
        Some(j) => {
            let state = j.dir().parent().map(Path::to_path_buf).unwrap_or_default();
            format!(".,{}//", state.join("backup").display())
        }
        None => ".".into(),
    }
}

/// The files whose visit has made its one Emacs backup (`buffer-backed-up`), by canonical path.
fn visits() -> &'static std::sync::Mutex<std::collections::HashSet<PathBuf>> {
    static V: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<PathBuf>>> =
        std::sync::OnceLock::new();
    V.get_or_init(Default::default)
}

fn visit_key(p: &Path) -> PathBuf {
    fs::canonicalize(p).unwrap_or_else(|_| absolute(p))
}

fn backed_up(p: &Path) -> bool {
    visits().lock().is_ok_and(|v| v.contains(&visit_key(p)))
}

fn mark_backed_up(p: &Path) {
    if let Ok(mut v) = visits().lock() {
        v.insert(visit_key(p));
    }
}

/// A new visit of `p` (opened or re-read): its next save makes an Emacs backup again.
pub fn visited(p: &Path) {
    if let Ok(mut v) = visits().lock() {
        v.remove(&visit_key(p));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "ruse-backup-{tag}-{}-{}",
            std::process::id(),
            N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        fs::create_dir_all(&d).unwrap();
        d
    }

    /// A write by rename, as a save makes one: a backup hard-linked to the old file keeps the old text.
    fn replace(f: &Path, text: &str) {
        let tmp = f.with_extension("ruse-test-tmp");
        fs::write(&tmp, text).unwrap();
        fs::rename(&tmp, f).unwrap();
    }

    fn vim(keep: bool, write: bool, dirs: &str) -> Policy {
        Policy {
            keep,
            write,
            once_per_visit: false,
            skip: String::new(),
            patchmode: String::new(),
            rules: Rules::Vim {
                dirs: dirs.into(),
                ext: "~".into(),
            },
        }
    }

    fn emacs(vc: VersionControl, old: usize, new: usize, del: DeleteOld) -> Policy {
        Policy {
            keep: true,
            write: false,
            once_per_visit: true,
            skip: String::new(),
            patchmode: String::new(),
            rules: Rules::Emacs(Emacs {
                alist: Vec::new(),
                version_control: vc,
                kept_old: old,
                kept_new: new,
                delete_old: del,
                temp_dirs: Vec::new(),
                copying: Copying::EMACS,
            }),
        }
    }

    #[test]
    fn globs_match_like_vim_file_patterns() {
        assert!(glob_match(b"/tmp/*", b"/tmp/a/b.txt"));
        assert!(!glob_match(b"/tmp/*", b"/tmpx/a"));
        assert!(glob_match(b"*.txt", b"/a/b.txt"));
        assert!(glob_match(b"/a/?.c", b"/a/x.c"));
        assert!(glob_match(b"/a/[xy].c", b"/a/y.c"));
        assert!(!glob_match(b"/a/[!xy].c", b"/a/y.c"));
        assert!(glob_match(b"*", b""));
    }

    #[test]
    fn expand_reads_the_environment() {
        assert_eq!(expand("a$RUSE_NO_SUCH_VAR_619/b"), "a/b");
        assert_eq!(expand("plain"), "plain");
        let home = std::env::var("HOME").unwrap_or_default();
        if !home.is_empty() {
            assert_eq!(expand("~/x"), format!("{home}/x"));
            assert_eq!(expand("${HOME}/x"), format!("{home}/x"));
        }
    }

    #[test]
    fn vim_backup_keeps_the_previous_text_every_write() {
        let d = tmp("vim");
        let f = d.join("f.txt");
        fs::write(&f, "one\n").unwrap();
        let p = vim(true, true, ".");
        let made = make(&p, &f, false).unwrap();
        assert_eq!(made.backup.as_deref(), Some(d.join("f.txt~").as_path()));
        fs::write(d.join("new"), "two\n").unwrap();
        fs::rename(d.join("new"), &f).unwrap();
        assert!(made.finish(true).is_none());
        assert_eq!(fs::read(d.join("f.txt~")).unwrap(), b"one\n");
        // The next write replaces the backup with the text before it.
        make(&p, &f, true).unwrap().finish(true);
        assert_eq!(fs::read(d.join("f.txt~")).unwrap(), b"two\n");
        fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn writebackup_guards_only_an_in_place_write() {
        let d = tmp("wb");
        let f = d.join("f.txt");
        fs::write(&f, "one\n").unwrap();
        let p = vim(false, true, ".");
        assert!(make(&p, &f, false).unwrap().backup.is_none());
        let made = make(&p, &f, true).unwrap();
        let b = made.backup.clone().unwrap();
        assert_eq!(fs::read(&b).unwrap(), b"one\n");
        let note = made.finish(false).unwrap();
        assert!(note.contains("f.txt~"), "{note}");
        assert!(b.exists(), "a failed write keeps its write backup");
        make(&p, &f, true).unwrap().finish(true);
        assert!(!b.exists(), "a successful write removes it");
        fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn backupdir_slashes_name_the_whole_path_and_missing_dirs_are_made() {
        let d = tmp("dir");
        let f = d.join("f.txt");
        fs::write(&f, "one\n").unwrap();
        let bk = d.join("bk");
        let p = vim(true, false, &format!("{}//", bk.display()));
        let b = make(&p, &f, false).unwrap().backup.unwrap();
        assert_eq!(b.parent(), Some(bk.as_path()));
        let name = b.file_name().unwrap().to_string_lossy().into_owned();
        assert!(name.starts_with('%') && name.ends_with("%f.txt~"), "{name}");
        // A directory that cannot be made falls through to the next entry.
        let blocker = d.join("file");
        fs::write(&blocker, "").unwrap();
        let p = vim(true, false, &format!("{}/sub,.", blocker.display()));
        let b = make(&p, &f, false).unwrap().backup.unwrap();
        assert_eq!(b, d.join("f.txt~"));
        fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn backupskip_and_patchmode() {
        let d = tmp("skip");
        let f = d.join("f.txt");
        fs::write(&f, "one\n").unwrap();
        let mut p = vim(true, true, ".");
        p.skip = format!("{}/*", d.display());
        assert!(make(&p, &f, false).unwrap().backup.is_none());
        p.skip = String::new();
        p.patchmode = ".orig".into();
        make(&p, &f, false).unwrap();
        assert_eq!(fs::read(d.join("f.txt.orig")).unwrap(), b"one\n");
        replace(&f, "two\n");
        make(&p, &f, true).unwrap();
        assert_eq!(
            fs::read(d.join("f.txt.orig")).unwrap(),
            b"one\n",
            "kept once"
        );
        let n = d.join("n.txt");
        patch_new(&p, &n);
        assert_eq!(fs::read(d.join("n.txt.orig")).unwrap(), b"");
        fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn emacs_backs_up_once_per_visit() {
        let d = tmp("visit");
        let f = d.join("f.txt");
        fs::write(&f, "one\n").unwrap();
        let p = emacs(VersionControl::Existing, 2, 2, DeleteOld::Yes);
        visited(&f);
        assert!(make(&p, &f, false).unwrap().backup.is_some());
        replace(&f, "two\n");
        assert!(make(&p, &f, false).unwrap().backup.is_none());
        assert_eq!(fs::read(d.join("f.txt~")).unwrap(), b"one\n");
        visited(&f);
        assert!(make(&p, &f, false).unwrap().backup.is_some());
        assert_eq!(fs::read(d.join("f.txt~")).unwrap(), b"two\n");
        fs::remove_dir_all(d).unwrap();
    }

    /// Emacs 31.1 with `version-control t`, `kept-old-versions 1`, `kept-new-versions 2`,
    /// `delete-old-versions t`, six visits: `f.txt.~1~ ~5~ ~6~` remain.
    #[test]
    fn emacs_numbered_versions_are_pruned_as_emacs_does() {
        let d = tmp("vc");
        let f = d.join("f.txt");
        fs::write(&f, "v0\n").unwrap();
        let p = emacs(VersionControl::Always, 1, 2, DeleteOld::Yes);
        for w in 1..=6 {
            visited(&f);
            make(&p, &f, false).unwrap();
            fs::remove_file(&f).unwrap();
            fs::write(&f, format!("v{w}\n")).unwrap();
        }
        let mut names: Vec<String> = fs::read_dir(&d)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names, ["f.txt", "f.txt.~1~", "f.txt.~5~", "f.txt.~6~"]);
        assert_eq!(fs::read(d.join("f.txt.~6~")).unwrap(), b"v5\n");
        // `nil` (Emacs asks) keeps them and says how many are over.
        let p = emacs(VersionControl::Always, 1, 1, DeleteOld::Ask);
        visited(&f);
        assert_eq!(make(&p, &f, false).unwrap().excess, 2);
        fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn emacs_version_control_nil_follows_existing_numbers_and_never_is_plain() {
        let d = tmp("vcnil");
        let f = d.join("f.txt");
        fs::write(&f, "v0\n").unwrap();
        fs::write(d.join("f.txt.~3~"), "x").unwrap();
        visited(&f);
        let b = make(
            &emacs(VersionControl::Existing, 2, 2, DeleteOld::Yes),
            &f,
            false,
        )
        .unwrap()
        .backup
        .unwrap();
        assert_eq!(b, d.join("f.txt.~4~"));
        visited(&f);
        let b = make(
            &emacs(VersionControl::Never, 2, 2, DeleteOld::Yes),
            &f,
            false,
        )
        .unwrap()
        .backup
        .unwrap();
        assert_eq!(b, d.join("f.txt~"));
        fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn emacs_alist_and_temp_dirs() {
        let d = tmp("alist");
        let f = d.join("f!x.txt");
        fs::write(&f, "v0\n").unwrap();
        let mut p = emacs(VersionControl::Existing, 2, 2, DeleteOld::Yes);
        let bk = d.join("bk");
        if let Rules::Emacs(e) = &mut p.rules {
            e.alist = vec![(".".into(), bk.display().to_string())];
        }
        visited(&f);
        let b = make(&p, &f, false).unwrap().backup.unwrap();
        let name = b.file_name().unwrap().to_string_lossy().into_owned();
        assert!(
            name.starts_with('!') && name.ends_with("!f!!x.txt~"),
            "{name}"
        );
        if let Rules::Emacs(e) = &mut p.rules {
            e.alist = vec![(".".into(), "rel".into())];
            e.temp_dirs = vec![d.clone()];
        }
        visited(&f);
        assert!(
            make(&p, &f, false).unwrap().backup.is_none(),
            "under a temp dir"
        );
        if let Rules::Emacs(e) = &mut p.rules {
            e.temp_dirs.clear();
        }
        visited(&f);
        assert_eq!(
            make(&p, &f, false).unwrap().backup.unwrap(),
            absolute(&d).join("rel").join("f!x.txt~")
        );
        fs::remove_dir_all(d).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn a_backup_never_replaces_a_symlink() {
        let d = tmp("link");
        let f = d.join("f.txt");
        fs::write(&f, "one\n").unwrap();
        let victim = d.join("victim");
        fs::write(&victim, "keep").unwrap();
        std::os::unix::fs::symlink(&victim, d.join("f.txt~")).unwrap();
        let err = make(&vim(true, false, "."), &f, false).unwrap_err();
        assert_eq!(
            err.to_string(),
            "E510: Can't make backup file (add ! to override)"
        );
        assert_eq!(fs::read(&victim).unwrap(), b"keep");
        fs::remove_dir_all(d).unwrap();
    }
}
