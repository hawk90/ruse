//! Sibling temp files: unique naming, creation and the sweep of stale ones. Split from `atomic.rs`
//! (#1395).

use super::*;

/// A random-enough 32-bit value without a crate: std's per-process random hash keys, salted with a
/// counter and the clock. Uniqueness is enforced by `O_EXCL`, not by this.
fn random_u32() -> u32 {
    use std::hash::{BuildHasher, Hasher};
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u64(N.fetch_add(1, Ordering::Relaxed));
    h.write_u32(std::process::id());
    if let Ok(d) = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        h.write_u128(d.as_nanos());
    }
    h.finish() as u32
}

/// `.<name>.ruse-tmp-<pid>-<rand>` next to `target`, with `<name>` shortened so the whole stays within
/// `NAME_MAX` (#868: a name near the limit could not be saved).
pub(super) fn temp_path(target: &Path, rand: u32) -> PathBuf {
    let mut tmp = temp_prefix(target);
    tmp.push(format!("{}-{rand:08x}", std::process::id()));
    target.with_file_name(tmp)
}

/// `.<name>.ruse-tmp-` of `target`'s temp names, `<name>` shortened as [`temp_path`] does.
fn temp_prefix(target: &Path) -> std::ffi::OsString {
    let room = NAME_MAX - 1 - TEMP_TAG.len() - TEMP_PID_LEN - TEMP_RAND_LEN;
    let name = target.file_name().unwrap_or_default();
    #[cfg(unix)]
    let stem = {
        use std::os::unix::ffi::OsStrExt;
        let b = name.as_bytes();
        std::ffi::OsStr::from_bytes(&b[..b.len().min(room)]).to_os_string()
    };
    #[cfg(not(unix))]
    let stem = {
        let s = name.to_string_lossy();
        let mut end = s.len().min(room);
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        std::ffi::OsString::from(&s[..end])
    };
    let mut tmp = std::ffi::OsString::from(".");
    tmp.push(stem);
    tmp.push(TEMP_TAG);
    tmp
}

/// The writer's pid in a temp name of `target` (`prefix` = [`temp_prefix`]): `None` unless the name is
/// exactly `<prefix><digits>-<8 hex digits>`.
pub(super) fn temp_writer(name: &[u8], prefix: &[u8]) -> Option<u32> {
    let rest = name.strip_prefix(prefix)?;
    let dash = rest.iter().position(|&b| b == b'-')?;
    let (pid, rand) = (&rest[..dash], &rest[dash + 1..]);
    let digits = !pid.is_empty() && pid.len() < TEMP_PID_LEN && pid.iter().all(u8::is_ascii_digit);
    let hex = rand.len() == TEMP_RAND_LEN
        && rand
            .iter()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(b));
    if !(digits && hex) {
        return None;
    }
    std::str::from_utf8(pid).ok()?.parse().ok()
}

/// Remove the temp files a crashed save of `target` left beside it (#1197): only names of exactly
/// `target`'s temp pattern ([`temp_writer`]), only regular files (never a symlink or anything else), and
/// only when their writer is not running (a pid of this host that `kill(pid, 0)` says is gone). A live
/// writer's temp, a name that merely looks alike, and an old temp without a pid are left alone. Done once
/// per target per process, so a save does not list a large directory every time.
pub(super) fn sweep_stale_temps(target: &Path) {
    use std::collections::HashSet;
    use std::sync::Mutex;
    static SWEPT: Mutex<Option<HashSet<PathBuf>>> = Mutex::new(None);
    {
        let Ok(mut swept) = SWEPT.lock() else {
            return;
        };
        if !swept
            .get_or_insert_with(HashSet::new)
            .insert(target.to_path_buf())
        {
            return;
        }
    }
    let prefix = crate::persist::journal::path_bytes(Path::new(&temp_prefix(target)));
    let me = std::process::id();
    let Ok(entries) = fs::read_dir(containing_dir(target)) else {
        return;
    };
    for e in entries.flatten() {
        let name = crate::persist::journal::path_bytes(Path::new(&e.file_name()));
        let Some(pid) = temp_writer(&name, &prefix) else {
            continue;
        };
        let is_file = fs::symlink_metadata(e.path()).is_ok_and(|m| m.file_type().is_file());
        if pid != me && is_file && !crate::persist::journal::alive(pid) {
            crate::log::stale_temp_removed(&e.path());
            let _ = fs::remove_file(e.path());
        }
    }
}

/// Create a fresh temp next to `target`, exclusively (`O_EXCL`), so it never follows a planted symlink and
/// never truncates another save's temp or a user's file (#868). Owner-only (`0600`) when it will receive an
/// existing file's mode, so the new bytes never sit in a wider-mode file.
pub(super) fn create_temp(target: &Path, owner_only: bool) -> io::Result<(PathBuf, File)> {
    let mut last = None;
    for _ in 0..TEMP_ATTEMPTS {
        let tmp = temp_path(target, random_u32());
        let mut opts = OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        if owner_only {
            std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
        }
        #[cfg(not(unix))]
        let _ = owner_only;
        match opts.open(&tmp) {
            Ok(f) => return Ok((tmp, f)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => last = Some(e),
            Err(e) => return Err(e),
        }
    }
    Err(last.unwrap_or_else(|| io::Error::other("no free temp name")))
}
