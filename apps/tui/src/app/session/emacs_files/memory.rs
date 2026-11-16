//! The sizes and free memory find-file reports (#619, #1503): `file-size-human-readable-iec` and
//! `warn-maybe-out-of-memory`, with the free RAM and swap read from the OS.

use crate::app::write_opts::EmacsFiles;

/// Emacs's `file-size-human-readable-iec`: powers of 1024, one decimal below 10 when it is not whole
/// (`1.5 KiB`, `10 MiB`, `500 B`).
pub(super) fn human_size(mut size: f64) -> String {
    let units = ["B", "KiB", "MiB", "GiB", "TiB", "PiB", "EiB", "ZiB", "YiB"];
    let mut i = 0;
    while size >= 1024.0 && i + 1 < units.len() {
        size /= 1024.0;
        i += 1;
    }
    let frac = size % 1.0;
    if size < 10.0 && (0.05..0.95).contains(&frac) {
        format!("{size:.1} {}", units[i])
    } else {
        format!("{size:.0} {}", units[i])
    }
}

/// Emacs's `warn-maybe-out-of-memory`: the warning when a file of `size` bytes is over
/// `out-of-memory-warning-percentage` of the free memory (RAM and swap), `None` otherwise or when the free
/// memory cannot be read.
pub(super) fn out_of_memory_warning(size: u64, vars: &EmacsFiles) -> Option<String> {
    let pct = vars.out_of_memory_warning_percentage?;
    if size == 0 {
        return None;
    }
    let free_kib = free_memory_kib()? as f64;
    if (size as f64 / 1024.0) <= free_kib * pct as f64 / 100.0 {
        return None;
    }
    Some(format!(
        "Warning (emacs): You are trying to open a file whose size ({})\nexceeds the {pct}% of currently \
         available free memory ({}).\nIf that fails, try to open it with `find-file-literally'\n(but note \
         that some characters might be displayed incorrectly).",
        human_size(size as f64),
        human_size(free_kib * 1024.0)
    ))
}

/// Free RAM plus free swap in KiB, as Emacs's `memory-info` counts it.
#[cfg(target_os = "linux")]
fn free_memory_kib() -> Option<u64> {
    // SAFETY: `sysinfo` fills the zeroed struct it is given.
    let mut info: libc::sysinfo = unsafe { std::mem::zeroed() };
    if unsafe { libc::sysinfo(&mut info) } != 0 {
        return None;
    }
    let unit = u64::from(info.mem_unit.max(1));
    Some((info.freeram as u64 + info.freeswap as u64) * unit / 1024)
}

#[cfg(target_os = "macos")]
fn free_memory_kib() -> Option<u64> {
    fn sysctl<T: Default>(name: &str) -> Option<T> {
        let name = std::ffi::CString::new(name).ok()?;
        let mut out = T::default();
        let mut len = std::mem::size_of::<T>();
        // SAFETY: `out` is `len` bytes, and the name is NUL-terminated.
        let rc = unsafe {
            libc::sysctlbyname(
                name.as_ptr(),
                (&mut out as *mut T).cast(),
                &mut len,
                std::ptr::null_mut(),
                0,
            )
        };
        (rc == 0).then_some(out)
    }
    let pages: u32 = sysctl("vm.page_free_count")?;
    // SAFETY: `sysconf` has no preconditions.
    let page = u64::try_from(unsafe { libc::sysconf(libc::_SC_PAGESIZE) }).ok()?;
    let swap: libc::xsw_usage = sysctl_swap()?;
    Some((u64::from(pages) * page + swap.xsu_avail) / 1024)
}

#[cfg(target_os = "macos")]
fn sysctl_swap() -> Option<libc::xsw_usage> {
    let name = std::ffi::CString::new("vm.swapusage").ok()?;
    // SAFETY: zeroed is a valid `xsw_usage`.
    let mut out: libc::xsw_usage = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::xsw_usage>();
    // SAFETY: `out` is `len` bytes, and the name is NUL-terminated.
    let rc = unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            (&mut out as *mut libc::xsw_usage).cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    (rc == 0).then_some(out)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn free_memory_kib() -> Option<u64> {
    None
}
