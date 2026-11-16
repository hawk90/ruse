use std::ffi::CString;
use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;
use std::os::raw::{c_char, c_void};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

/// Attributes the kernel itself attaches to every file a process creates: neither copyable nor lost.
const KERNEL_MANAGED: &[&[u8]] = &[b"com.apple.provenance"];

/// Grow-and-retry for the list/get size race (an attribute changing between the two calls).
const RETRIES: usize = 4;

#[cfg(target_os = "macos")]
unsafe fn list(p: *const c_char, buf: *mut c_char, n: usize) -> isize {
    libc::listxattr(p, buf, n, 0)
}
#[cfg(not(target_os = "macos"))]
unsafe fn list(p: *const c_char, buf: *mut c_char, n: usize) -> isize {
    libc::listxattr(p, buf, n)
}
#[cfg(target_os = "macos")]
unsafe fn get(p: *const c_char, name: *const c_char, buf: *mut c_void, n: usize) -> isize {
    libc::getxattr(p, name, buf, n, 0, 0)
}
#[cfg(not(target_os = "macos"))]
unsafe fn get(p: *const c_char, name: *const c_char, buf: *mut c_void, n: usize) -> isize {
    libc::getxattr(p, name, buf, n)
}
#[cfg(target_os = "macos")]
unsafe fn fget(fd: i32, name: *const c_char, buf: *mut c_void, n: usize) -> isize {
    libc::fgetxattr(fd, name, buf, n, 0, 0)
}
#[cfg(not(target_os = "macos"))]
unsafe fn fget(fd: i32, name: *const c_char, buf: *mut c_void, n: usize) -> isize {
    libc::fgetxattr(fd, name, buf, n)
}
#[cfg(target_os = "macos")]
unsafe fn fset(fd: i32, name: *const c_char, val: *const c_void, n: usize) -> i32 {
    libc::fsetxattr(fd, name, val, n, 0, 0)
}
#[cfg(not(target_os = "macos"))]
unsafe fn fset(fd: i32, name: *const c_char, val: *const c_void, n: usize) -> i32 {
    libc::fsetxattr(fd, name, val, n, 0)
}

/// Read a variable-size value with the size-query-then-fetch protocol. `None`: the call reported
/// the attribute (or attributes) unsupported or absent.
fn read_sized(mut call: impl FnMut(*mut c_void, usize) -> isize) -> io::Result<Option<Vec<u8>>> {
    for _ in 0..RETRIES {
        let n = call(std::ptr::null_mut(), 0);
        if n < 0 {
            return absent_or_err();
        }
        let mut buf = vec![0u8; n as usize];
        let got = call(buf.as_mut_ptr().cast(), buf.len());
        if got >= 0 {
            buf.truncate(got as usize);
            return Ok(Some(buf));
        }
        if io::Error::last_os_error().raw_os_error() != Some(libc::ERANGE) {
            return absent_or_err();
        }
    }
    Err(io::Error::other("extended attributes kept changing"))
}

fn absent_or_err() -> io::Result<Option<Vec<u8>>> {
    let e = io::Error::last_os_error();
    #[cfg(target_os = "macos")]
    let absent = [libc::ENOTSUP, libc::ENOATTR];
    #[cfg(not(target_os = "macos"))]
    let absent = [libc::ENOTSUP, libc::ENODATA];
    if e.raw_os_error().is_some_and(|c| absent.contains(&c)) {
        Ok(None)
    } else {
        Err(e)
    }
}

pub(super) fn copy(src: &Path, dst: &File) -> io::Result<()> {
    let path = CString::new(src.as_os_str().as_bytes()).map_err(io::Error::other)?;
    let fd = dst.as_raw_fd();
    // SAFETY (all calls below): `path`/`name` are valid NUL-terminated strings, `fd` is open for the
    // lifetime of `dst`, and each buffer pointer comes with its true length.
    let Some(names) = read_sized(|b, n| unsafe { list(path.as_ptr(), b.cast(), n) })? else {
        return Ok(());
    };
    for raw in names.split(|&b| b == 0).filter(|n| !n.is_empty()) {
        if KERNEL_MANAGED.contains(&raw) {
            continue;
        }
        let name = CString::new(raw).map_err(io::Error::other)?;
        let Some(value) = read_sized(|b, n| unsafe { get(path.as_ptr(), name.as_ptr(), b, n) })?
        else {
            continue; // removed meanwhile
        };
        // Already right (an SELinux label the directory gives every new file): nothing to set.
        if read_sized(|b, n| unsafe { fget(fd, name.as_ptr(), b, n) })?.as_deref()
            == Some(&value[..])
        {
            continue;
        }
        if unsafe { fset(fd, name.as_ptr(), value.as_ptr().cast(), value.len()) } != 0 {
            return Err(io::Error::other(format!(
                "cannot copy extended attribute {}: {}",
                String::from_utf8_lossy(raw),
                io::Error::last_os_error()
            )));
        }
    }
    Ok(())
}
