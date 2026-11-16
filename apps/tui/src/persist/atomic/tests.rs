//! The durable-save tests. Split from `atomic.rs` (#1395).

use super::*;
use std::io::Read;

fn tmpdir() -> crate::TestDir {
    crate::TestDir::new("atomic")
}

fn read(p: &Path) -> Vec<u8> {
    let mut v = Vec::new();
    File::open(p).unwrap().read_to_end(&mut v).unwrap();
    v
}

/// The temp files left in `dir`.
fn temps(dir: &Path) -> Vec<std::ffi::OsString> {
    fs::read_dir(dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name())
        .filter(|n| n.to_string_lossy().contains(TEMP_TAG))
        .collect()
}

#[cfg(unix)]
fn is_root() -> bool {
    // SAFETY: geteuid has no preconditions.
    unsafe { libc::geteuid() == 0 }
}

#[cfg(unix)]
fn ino(p: &Path) -> u64 {
    std::os::unix::fs::MetadataExt::ino(&fs::metadata(p).unwrap())
}

/// Clears an injected fault even when the test panics.
struct Fault;
impl Fault {
    fn set(step: Step) -> Fault {
        inject(Some(step));
        Fault
    }
}
impl Drop for Fault {
    fn drop(&mut self) {
        inject(None);
    }
}

#[test]
fn writes_new_file_durably() {
    let dir = tmpdir();
    let f = dir.join("new.txt");
    take_synced_dirs();
    assert_eq!(write(&f, b"hello", false).unwrap(), Method::Renamed);
    assert_eq!(read(&f), b"hello");
    assert!(temps(&dir).is_empty(), "temp gone after a successful save");
    #[cfg(unix)]
    assert_eq!(
        take_synced_dirs(),
        vec![dir.to_path_buf()],
        "the rename is fsynced"
    );
}

#[test]
fn replaces_existing_atomically() {
    let dir = tmpdir();
    let f = dir.join("f.txt");
    save(&f, b"v1").unwrap();
    save(&f, b"v2 longer content").unwrap();
    assert_eq!(read(&f), b"v2 longer content");
    save(&f, b"v3").unwrap();
    assert_eq!(read(&f), b"v3", "a shorter write leaves no old tail");
    assert!(temps(&dir).is_empty());
}

/// #865: a bare relative name's directory is `.`, which is fsynced, not skipped as an empty path.
#[test]
fn bare_name_fsyncs_the_current_directory() {
    assert_eq!(containing_dir(Path::new("foo")), PathBuf::from("."));
    assert_eq!(containing_dir(Path::new("a/foo")), PathBuf::from("a"));
    assert_eq!(containing_dir(Path::new("/foo")), PathBuf::from("/"));
    #[cfg(unix)]
    {
        take_synced_dirs();
        sync_dir(&containing_dir(Path::new("foo"))).unwrap();
        assert_eq!(take_synced_dirs(), vec![PathBuf::from(".")]);
    }
}

/// #936: a failed directory open or fsync is an error, not a silent success: the file has the new bytes
/// but the save says it is not proven durable.
#[cfg(unix)]
#[test]
fn directory_fsync_failure_is_reported() {
    for step in [Step::OpenDir, Step::SyncDir] {
        let dir = tmpdir();
        let f = dir.join("f.txt");
        fs::write(&f, b"old").unwrap();
        let e = {
            let _g = Fault::set(step);
            write(&f, b"new", false).unwrap_err()
        };
        assert!(
            matches!(&e, SaveError::NotDurable { path, .. } if **path == *dir),
            "{step:?}: {e:?}"
        );
        assert!(e.to_string().starts_with("E667: Fsync failed"), "{e}");
        assert_eq!(read(&f), b"new", "{step:?}: the rename already happened");
        assert!(temps(&dir).is_empty());
    }
}

/// A filesystem that does not support a directory fsync (EINVAL, ENOTSUP, EOPNOTSUPP, EISDIR from the
/// open or the fsync) does not fail the save: it succeeds with a method whose note says the rename's
/// durability is not proven. A real I/O error (EIO, ENOSPC, EROFS, EACCES) stays E667.
#[cfg(unix)]
#[test]
fn unsupported_directory_fsync_is_saved_with_a_note() {
    let codes = [libc::EINVAL, libc::ENOTSUP, libc::EOPNOTSUPP, libc::EISDIR];
    for step in [Step::OpenDir, Step::SyncDir] {
        for code in codes {
            let dir = tmpdir();
            let f = dir.join("f.txt");
            fs::write(&f, b"old").unwrap();
            let m = {
                let _g = Fault::set(step);
                inject_errno(step, code);
                write(&f, b"new", false)
            };
            let m = m.unwrap_or_else(|e| panic!("{step:?} errno {code}: {e}"));
            assert_eq!(m, Method::RenamedDirSyncUnsupported, "{step:?} {code}");
            assert!(m.note().contains("rename durability not proven"));
            assert_eq!(read(&f), b"new");
        }
        for code in [libc::EIO, libc::ENOSPC, libc::EROFS, libc::EACCES] {
            let dir = tmpdir();
            let f = dir.join("f.txt");
            let e = {
                let _g = Fault::set(step);
                inject_errno(step, code);
                write(&f, b"new", false).unwrap_err()
            };
            assert!(
                matches!(e, SaveError::NotDurable { .. }),
                "{step:?} {code}: {e:?}"
            );
            assert!(e.to_string().starts_with("E667: "), "{e}");
        }
    }
    assert_eq!(Method::Renamed.note(), "");
    assert_eq!(Method::InPlace.note(), "");
}

/// A failed file fsync leaves the target untouched and no temp behind.
#[test]
fn file_fsync_failure_leaves_the_target_untouched() {
    let dir = tmpdir();
    let f = dir.join("f.txt");
    fs::write(&f, b"old").unwrap();
    let e = {
        let _g = Fault::set(Step::SyncFile);
        write(&f, b"new", false).unwrap_err()
    };
    assert!(matches!(e, SaveError::Untouched(_)), "{e:?}");
    assert_eq!(read(&f), b"old");
    assert!(temps(&dir).is_empty());
}

/// #868: concurrent saves of one path each get their own temp, so none fails; the file ends whole.
#[test]
fn concurrent_saves_all_succeed() {
    let dir = tmpdir();
    let f = dir.join("f.txt");
    let a = vec![b'a'; 4096];
    let b = vec![b'b'; 8192];
    std::thread::scope(|s| {
        for bytes in [&a, &b] {
            let f = &f;
            s.spawn(move || {
                for _ in 0..100 {
                    save(f, bytes).unwrap();
                }
            });
        }
    });
    let got = read(&f);
    assert!(got == a || got == b, "a whole version, {} bytes", got.len());
    assert!(temps(&dir).is_empty());
}

/// #868: a user's own files that look like temps are never deleted or followed.
#[cfg(unix)]
#[test]
fn files_named_like_temps_are_left_alone() {
    let dir = tmpdir();
    let victim = dir.join("victim");
    fs::write(&victim, b"keep").unwrap();
    let f = dir.join("f.txt");
    fs::write(dir.join("f.txt.ruse-tmp"), b"mine").unwrap();
    std::os::unix::fs::symlink(&victim, dir.join(".f.txt.ruse-tmp")).unwrap();
    save(&f, b"data").unwrap();
    assert_eq!(read(&dir.join("f.txt.ruse-tmp")), b"mine");
    assert!(fs::symlink_metadata(dir.join(".f.txt.ruse-tmp"))
        .unwrap()
        .file_type()
        .is_symlink());
    assert_eq!(read(&victim), b"keep");
    assert_eq!(read(&f), b"data");
}

/// #1197: a save removes the temp files a crashed save of the same file left (writer gone), and nothing
/// else: not a live writer's, not this process's, not a symlink, not a name that only looks alike.
#[cfg(unix)]
#[test]
fn a_save_sweeps_only_dead_writers_temps() {
    let dir = tmpdir();
    let f = dir.join("f.txt");
    fs::write(&f, b"old").unwrap();
    let gone = crate::persist::journal::GONE_PID;
    let mut child = std::process::Command::new("sleep")
        .arg("30")
        .spawn()
        .unwrap();
    let me = std::process::id();
    let dead = format!(".f.txt.ruse-tmp-{gone}-deadbeef");
    let kept = [
        format!(".f.txt.ruse-tmp-{}-0badf00d", child.id()), // a live writer
        format!(".f.txt.ruse-tmp-{me}-0badf00d"),           // this process
        format!(".f.txt.ruse-tmp-{gone}-deadbee"),          // 7 hex digits
        format!(".f.txt.ruse-tmp-{gone}-deadbeeg"),         // not hex
        format!(".f.txt.ruse-tmp-{gone}-deadbeef.bak"),
        ".f.txt.ruse-tmp-deadbeef".to_string(), // the old name, without a pid
        format!(".g.txt.ruse-tmp-{gone}-deadbeef"), // another file's
        format!("f.txt.ruse-tmp-{gone}-deadbeef"), // not hidden
    ];
    fs::write(dir.join(&dead), b"leftover").unwrap();
    for k in &kept {
        fs::write(dir.join(k), b"keep").unwrap();
    }
    let link = format!(".f.txt.ruse-tmp-{gone}-0000aaaa");
    std::os::unix::fs::symlink(dir.join("f.txt"), dir.join(&link)).unwrap();
    save(&f, b"new").unwrap();
    let _ = child.kill();
    let _ = child.wait();
    assert!(
        !dir.join(&dead).exists(),
        "the dead writer's temp is removed"
    );
    for k in &kept {
        assert_eq!(read(&dir.join(k)), b"keep", "{k} is kept");
    }
    assert!(
        fs::symlink_metadata(dir.join(&link)).is_ok(),
        "a symlink is kept"
    );
    assert_eq!(read(&f), b"new");
    assert_eq!(
        temp_writer(dead.as_bytes(), b".f.txt.ruse-tmp-"),
        Some(gone)
    );
    assert_eq!(
        temp_writer(b".f.txt.ruse-tmp-5-DEADBEEF", b".f.txt.ruse-tmp-"),
        None
    );
    assert_eq!(
        temp_writer(b".f.txt.ruse-tmp--deadbeef", b".f.txt.ruse-tmp-"),
        None
    );
}

/// #868: a name at NAME_MAX saves; its temp name is shortened to fit.
#[test]
fn longest_name_saves() {
    let dir = tmpdir();
    let f = dir.join("n".repeat(NAME_MAX));
    assert!(temp_path(&f, 0).file_name().unwrap().len() <= NAME_MAX);
    assert!(temp_path(&f, 7)
        .file_name()
        .unwrap()
        .to_string_lossy()
        .ends_with(&format!(".ruse-tmp-{}-00000007", std::process::id())));
    save(&f, b"x").unwrap();
    save(&f, b"y").unwrap();
    assert_eq!(read(&f), b"y");
}

/// Regression: saving keeps the original file's permission bits.
#[cfg(unix)]
#[test]
fn preserves_permission_bits() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tmpdir();
    for mode in [0o755, 0o600, 0o640] {
        let f = dir.join(format!("m{mode:o}"));
        fs::write(&f, b"#!/bin/sh\n").unwrap();
        fs::set_permissions(&f, fs::Permissions::from_mode(mode)).unwrap();
        save(&f, b"#!/bin/sh\necho hi\n").unwrap();
        let got = fs::metadata(&f).unwrap().permissions().mode() & 0o7777;
        assert_eq!(got, mode, "mode {mode:o} preserved across save");
    }
}

/// #864: a hard-linked file is written in place, so every name sees the new bytes (nvim does the same).
#[cfg(unix)]
#[test]
fn hard_link_is_written_in_place() {
    let dir = tmpdir();
    let f = dir.join("f");
    let g = dir.join("g");
    fs::write(&f, b"v1\n").unwrap();
    fs::hard_link(&f, &g).unwrap();
    let before = ino(&f);
    assert_eq!(write(&f, b"v2 longer\n", false).unwrap(), Method::InPlace);
    assert_eq!(read(&g), b"v2 longer\n");
    assert_eq!(ino(&f), before);
    assert_eq!(write(&g, b"v3\n", false).unwrap(), Method::InPlace);
    assert_eq!(read(&f), b"v3\n", "a shorter write truncates the old tail");
    assert!(temps(&dir).is_empty());
}

/// #864: a file the user cannot write is refused with E505 unless forced; forced, its mode is kept.
#[cfg(unix)]
#[test]
fn read_only_file_needs_force() {
    use std::os::unix::fs::PermissionsExt;
    if is_root() {
        return;
    }
    let dir = tmpdir();
    let f = dir.join("r");
    fs::write(&f, b"ro\n").unwrap();
    fs::set_permissions(&f, fs::Permissions::from_mode(0o444)).unwrap();
    let e = write(&f, b"rox\n", false).unwrap_err();
    assert!(matches!(e, SaveError::ReadOnly(_)), "{e:?}");
    assert!(e.to_string().starts_with("E505: "), "{e}");
    assert_eq!(read(&f), b"ro\n");
    write(&f, b"rox\n", true).unwrap();
    assert_eq!(read(&f), b"rox\n");
    assert_eq!(
        fs::metadata(&f).unwrap().permissions().mode() & 0o777,
        0o444
    );
    // The same through the in-place path (a hard link), which must add and then drop write permission.
    fs::hard_link(&f, dir.join("r2")).unwrap();
    assert!(write(&f, b"x\n", false).is_err());
    assert_eq!(write(&f, b"roxx\n", true).unwrap(), Method::InPlace);
    assert_eq!(read(&dir.join("r2")), b"roxx\n");
    assert_eq!(
        fs::metadata(&f).unwrap().permissions().mode() & 0o777,
        0o444
    );
}

/// #864: a writable file in a directory that is not writable is written in place (nvim does too) instead
/// of failing; a NEW file there still fails cleanly.
#[cfg(unix)]
#[test]
fn unwritable_directory_falls_back_to_in_place() {
    use std::os::unix::fs::PermissionsExt;
    if is_root() {
        return;
    }
    let dir = tmpdir();
    let f = dir.join("f");
    fs::write(&f, b"old\n").unwrap();
    let before = ino(&f);
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o555)).unwrap();
    let r = write(&f, b"new\n", false);
    let fresh = write(&dir.join("g"), b"x", false);
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(r.unwrap(), Method::InPlace);
    assert_eq!(read(&f), b"new\n");
    assert_eq!(ino(&f), before);
    assert!(matches!(fresh, Err(SaveError::Untouched(_))), "{fresh:?}");
}

/// An in-place write that fails part-way says the file may be partly written.
#[cfg(unix)]
#[test]
fn in_place_failure_is_reported_as_partial() {
    let dir = tmpdir();
    let f = dir.join("f");
    fs::write(&f, b"v1\n").unwrap();
    fs::hard_link(&f, dir.join("g")).unwrap();
    let e = {
        let _g = Fault::set(Step::InPlaceWrite);
        write(&f, b"v2\n", false).unwrap_err()
    };
    assert!(matches!(e, SaveError::Partial { .. }), "{e:?}");
    assert!(e.to_string().contains("may be partly written"), "{e}");
    let e = {
        let _g = Fault::set(Step::SyncFile);
        write(&f, b"v2\n", false).unwrap_err()
    };
    assert!(matches!(e, SaveError::NotDurable { .. }), "{e:?}");
}

/// #935: a user extended attribute survives the save (by copying it to the new inode).
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn extended_attributes_survive() {
    use std::os::unix::ffi::OsStrExt;
    let dir = tmpdir();
    let f = dir.join("x");
    fs::write(&f, b"x\n").unwrap();
    let c = std::ffi::CString::new(f.as_os_str().as_bytes()).unwrap();
    let name = c"user.ruse-test";
    let val = b"value";
    // SAFETY: valid NUL-terminated strings and a buffer with its true length.
    #[cfg(target_os = "macos")]
    let rc = unsafe { libc::setxattr(c.as_ptr(), name.as_ptr(), val.as_ptr().cast(), 5, 0, 0) };
    #[cfg(target_os = "linux")]
    let rc = unsafe { libc::setxattr(c.as_ptr(), name.as_ptr(), val.as_ptr().cast(), 5, 0) };
    if rc != 0 {
        return; // the filesystem has no user xattrs (tmpfs on an old kernel)
    }
    let before = ino(&f);
    // The rename path copies it (an in-place write would keep it trivially).
    assert_eq!(write(&f, b"xy\n", false).unwrap(), Method::Renamed);
    assert_ne!(ino(&f), before);
    assert_eq!(read(&f), b"xy\n");
    let mut buf = [0u8; 16];
    // SAFETY: as above.
    #[cfg(target_os = "macos")]
    let n = unsafe { libc::getxattr(c.as_ptr(), name.as_ptr(), buf.as_mut_ptr().cast(), 16, 0, 0) };
    #[cfg(target_os = "linux")]
    let n = unsafe { libc::getxattr(c.as_ptr(), name.as_ptr(), buf.as_mut_ptr().cast(), 16) };
    assert_eq!(n, 5, "the xattr is still there");
    assert_eq!(&buf[..5], val);
}

/// Regression: saving through a symlink writes the link's TARGET and leaves the symlink in place.
/// Covers a relative link and a two-hop chain.
#[cfg(unix)]
#[test]
fn writes_through_symlinks() {
    use std::os::unix::fs::symlink;
    let dir = tmpdir();
    let real = dir.join("real.txt");
    fs::write(&real, b"old").unwrap();
    let link = dir.join("link.txt");
    symlink("real.txt", &link).unwrap(); // relative target
    let link2 = dir.join("link2.txt");
    symlink(&link, &link2).unwrap(); // link → link → file

    save(&link2, b"new").unwrap();
    assert_eq!(read(&real), b"new", "the target file got the bytes");
    for l in [&link, &link2] {
        assert!(
            fs::symlink_metadata(l).unwrap().file_type().is_symlink(),
            "{} is still a symlink",
            l.display()
        );
    }
    assert!(temps(&dir).is_empty());
}

/// A symlink cycle is an error, not an infinite loop.
#[cfg(unix)]
#[test]
fn symlink_cycle_is_an_error() {
    let dir = tmpdir();
    let a = dir.join("a");
    let b = dir.join("b");
    std::os::unix::fs::symlink(&b, &a).unwrap();
    std::os::unix::fs::symlink(&a, &b).unwrap();
    assert!(save(&a, b"x").is_err());
}

/// #907: saving over a FIFO, a socket or a directory refuses, rather than renaming a regular file over
/// the node.
#[cfg(unix)]
#[test]
fn non_regular_target_is_refused_and_kept() {
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::FileTypeExt;
    let dir = tmpdir();
    let fifo = dir.join("p");
    let c = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
    // SAFETY: `c` is a valid NUL-terminated path.
    assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
    let e = save(&fifo, b"x").unwrap_err();
    assert!(e.to_string().contains("is not a regular file"), "{e}");
    assert!(fs::metadata(&fifo).unwrap().file_type().is_fifo());
    let sock = dir.join("s");
    let _l = std::os::unix::net::UnixListener::bind(&sock).unwrap();
    let e = write(&sock, b"x", true).unwrap_err();
    assert!(matches!(e, SaveError::NotRegular(_)), "{e:?}");
    assert!(fs::metadata(&sock).unwrap().file_type().is_socket());
    let sub = dir.join("sub");
    fs::create_dir(&sub).unwrap();
    assert!(save(&sub, b"x").is_err());
    assert!(sub.is_dir());
    assert!(temps(&dir).is_empty());
}

fn vim_backups(keep: bool, write: bool) -> super::super::backup::Policy {
    super::super::backup::Policy {
        keep,
        write,
        once_per_visit: false,
        skip: String::new(),
        patchmode: String::new(),
        rules: super::super::backup::Rules::Vim {
            dirs: ".".into(),
            ext: "~".into(),
        },
    }
}

/// nvim v0.12.6 `'backupcopy'`: `yes` overwrites a hard-linked file in place (both names see the new
/// text, the backup is a copy); `no` renames, so the other name keeps the old text; `auto,breakhardlink`
/// renames too.
#[cfg(unix)]
#[test]
fn backupcopy_decides_the_write_method() {
    for (copy, brk, linked_sees_new) in [
        (Copy::Yes, false, true),
        (Copy::No, false, false),
        (Copy::Auto, true, false),
        (Copy::Auto, false, true),
    ] {
        let dir = tmpdir();
        let f = dir.join("h.txt");
        let l = dir.join("l.txt");
        fs::write(&f, b"one\n").unwrap();
        fs::hard_link(&f, &l).unwrap();
        let policy = vim_backups(true, true);
        let opts = Opts {
            copy,
            break_hardlink: brk,
            backup: Some(&policy),
            ..Opts::new(false)
        };
        let w = write_with(&f, b"TWO\n", &opts).unwrap();
        assert_eq!(read(&f), b"TWO\n");
        let want: &[u8] = if linked_sees_new { b"TWO\n" } else { b"one\n" };
        assert_eq!(read(&l), want, "{copy:?} breakhardlink={brk}");
        assert_eq!(read(&dir.join("h.txt~")), b"one\n", "{copy:?}");
        assert_eq!(w.backup.as_deref(), Some(dir.join("h.txt~").as_path()));
        assert!(temps(&dir).is_empty());
    }
}

/// `'backupcopy'` `breaksymlink`: the link becomes a regular file and its target keeps the old text.
#[cfg(unix)]
#[test]
fn breaksymlink_replaces_the_link() {
    let dir = tmpdir();
    let t = dir.join("t.txt");
    let s = dir.join("s.txt");
    fs::write(&t, b"one\n").unwrap();
    std::os::unix::fs::symlink("t.txt", &s).unwrap();
    let opts = Opts {
        break_symlink: true,
        ..Opts::new(false)
    };
    write_with(&s, b"TWO\n", &opts).unwrap();
    assert!(fs::symlink_metadata(&s).unwrap().file_type().is_file());
    assert_eq!(read(&s), b"TWO\n");
    assert_eq!(read(&t), b"one\n");
}

/// A write backup guards an in-place write and is gone after it; with `'nowritebackup'` there is none.
#[cfg(unix)]
#[test]
fn writebackup_is_removed_after_an_in_place_write() {
    let dir = tmpdir();
    let f = dir.join("f.txt");
    fs::write(&f, b"one\n").unwrap();
    let policy = vim_backups(false, true);
    let opts = Opts {
        copy: Copy::Yes,
        backup: Some(&policy),
        ..Opts::new(false)
    };
    let w = write_with(&f, b"two\n", &opts).unwrap();
    assert_eq!(w.backup, None);
    assert!(!dir.join("f.txt~").exists());
    assert_eq!(read(&f), b"two\n");
}

/// nvim: a backup that cannot be made stops the write (E510) and leaves the file alone; `:w!` writes
/// without it.
#[cfg(unix)]
#[test]
fn a_backup_that_cannot_be_made_needs_force() {
    let dir = tmpdir();
    let f = dir.join("f.txt");
    fs::write(&f, b"one\n").unwrap();
    let blocker = dir.join("file");
    fs::write(&blocker, b"").unwrap();
    let mut policy = vim_backups(true, true);
    policy.rules = super::super::backup::Rules::Vim {
        dirs: format!("{}/sub", blocker.display()),
        ext: "~".into(),
    };
    let mut opts = Opts {
        backup: Some(&policy),
        ..Opts::new(false)
    };
    let e = write_with(&f, b"two\n", &opts).unwrap_err();
    assert_eq!(
        e.to_string(),
        "E510: Can't make backup file (add ! to override)"
    );
    assert_eq!(read(&f), b"one\n");
    opts.force = true;
    write_with(&f, b"two\n", &opts).unwrap();
    assert_eq!(read(&f), b"two\n");
}

/// `'nofsync'`: the write still lands, without the file or directory sync.
#[cfg(unix)]
#[test]
fn nofsync_skips_the_syncs() {
    let dir = tmpdir();
    let f = dir.join("f.txt");
    fs::write(&f, b"one\n").unwrap();
    take_synced_dirs();
    let opts = Opts {
        fsync: false,
        ..Opts::new(false)
    };
    write_with(&f, b"two\n", &opts).unwrap();
    assert_eq!(read(&f), b"two\n");
    assert!(take_synced_dirs().is_empty());
}
