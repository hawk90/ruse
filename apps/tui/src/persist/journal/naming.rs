//! Journal file names that fit `NAME_MAX`. Split from `journal.rs` (#1389).

use super::{fs, is_live_other, owner, read_head, rj01, Path, PathBuf};

/// The journal this process may write for a document whose journal is `journal`: that file, unless a live
/// other session owns it (#1193), a newer ruse wrote it (kept for that version, RFC-0023 §7), or it is a
/// regular file this process cannot read (mode 000, another user's: it may hold a crash's unsaved work, and
/// the open reported it as kept), in which case this process's own `<name>.<pid>.rj` beside it. Anything
/// else at the path that is not a regular file (a FIFO, a device) keeps the name, and the write fails
/// visibly (#869).
pub fn claim(journal: &Path) -> PathBuf {
    match read_head(journal) {
        Some(Ok((rj01::Record::Header(h), _))) if is_live_other(&h.owner) => {
            alternate(journal, owner().pid)
        }
        Some(Err(rj01::Stop::NewerMajor(_))) => alternate(journal, owner().pid),
        None if fs::symlink_metadata(journal).is_ok_and(|m| m.is_file()) => {
            alternate(journal, owner().pid)
        }
        _ => journal.to_path_buf(),
    }
}

/// `<stem>.<pid>.rj` for the journal `<stem>.rj`.
pub(super) fn alternate(journal: &Path, pid: u32) -> PathBuf {
    let name = journal
        .file_name()
        .map_or_else(Vec::new, |n| path_bytes(Path::new(n)));
    let stem = name.strip_suffix(b".rj").unwrap_or(&name);
    let mut alt = stem.to_vec();
    alt.extend_from_slice(format!(".{pid}.rj").as_bytes());
    journal.with_file_name(os_name(alt))
}

pub(super) fn host_name() -> Vec<u8> {
    #[cfg(unix)]
    {
        let mut buf = [0u8; 256];
        // SAFETY: `buf` is a valid writable buffer of the given length.
        if unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) } == 0 {
            let n = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
            return buf[..n].to_vec();
        }
    }
    Vec::new()
}

/// A path's raw bytes, as the journal Header stores them.
pub fn path_bytes(p: &Path) -> Vec<u8> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        p.as_os_str().as_bytes().to_vec()
    }
    #[cfg(not(unix))]
    {
        p.to_string_lossy().into_owned().into_bytes()
    }
}

/// The canonical absolute path a journal is keyed by: symlinks resolved, and for a file that does not
/// exist yet, its canonical directory joined with its name. Falls back to the plain absolute path.
pub fn canonical(doc: &Path) -> PathBuf {
    if let Ok(c) = fs::canonicalize(doc) {
        return c;
    }
    let abs = std::path::absolute(doc).unwrap_or_else(|_| doc.to_path_buf());
    match (abs.parent(), abs.file_name()) {
        (Some(dir), Some(name)) => fs::canonicalize(dir).map_or(abs.clone(), |d| d.join(name)),
        _ => abs,
    }
}

/// Shorten `stem` (at a character boundary when it is UTF-8) to at most `room` bytes.
pub(super) fn shorten(stem: &[u8], room: usize) -> &[u8] {
    if stem.len() <= room {
        return stem;
    }
    let mut end = room;
    if std::str::from_utf8(stem).is_ok() {
        while end > 0 && (stem[end] & 0xC0) == 0x80 {
            end -= 1;
        }
    }
    &stem[..end]
}

#[cfg(unix)]
pub(super) fn os_name(bytes: Vec<u8>) -> std::ffi::OsString {
    use std::os::unix::ffi::OsStringExt;
    std::ffi::OsString::from_vec(bytes)
}

#[cfg(not(unix))]
pub(super) fn os_name(bytes: Vec<u8>) -> std::ffi::OsString {
    String::from_utf8_lossy(&bytes).into_owned().into()
}
