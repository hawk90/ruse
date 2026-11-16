//! Who owns a journal, and whether that process is alive. Split from `journal.rs` (#1389).

use super::*;

/// Unix milliseconds now.
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// This process as a journal owner: pid, host name, and its start time as the system reports it
/// ([`process_start_ms`]), so a later session can tell this process from another that reuses its pid. When
/// the system does not say, the time of the first call stands in (the session asks at startup, so the
/// unnamed buffer's journal name is fixed then); such a journal is never taken for a live one.
pub fn owner() -> &'static Owner {
    static OWNER: OnceLock<Owner> = OnceLock::new();
    OWNER.get_or_init(|| {
        let pid = std::process::id();
        Owner {
            pid,
            host: host_name(),
            start_ms: process_start_ms(pid).unwrap_or_else(now_ms),
        }
    })
}

/// How far apart two readings of one process's start time may be: Linux derives it from the boot time,
/// which is kept in whole seconds and moves when the clock is stepped.
pub(super) const START_SLACK_MS: u64 = 2000;

/// The start time of process `pid`, Unix milliseconds, as the system reports it: `/proc/<pid>/stat` and
/// the boot time on Linux, `proc_pidinfo` on macOS. `None` when it cannot be read.
pub fn process_start_ms(pid: u32) -> Option<u64> {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        // The command name (field 2) may hold spaces and parentheses: count fields after the last `)`.
        let rest = &stat[stat.rfind(')')? + 1..];
        let ticks: u64 = rest.split_whitespace().nth(19)?.parse().ok()?;
        let btime: u64 = fs::read_to_string("/proc/stat")
            .ok()?
            .lines()
            .find_map(|l| l.strip_prefix("btime "))?
            .trim()
            .parse()
            .ok()?;
        // SAFETY: sysconf has no preconditions.
        let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
        if hz <= 0 {
            return None;
        }
        Some(btime * 1000 + ticks * 1000 / hz as u64)
    }
    #[cfg(target_os = "macos")]
    {
        let pid = libc::c_int::try_from(pid).ok()?;
        // SAFETY: plain data, fully written by proc_pidinfo on success.
        let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
        // SAFETY: `info` is a valid buffer of `size` bytes for the PROC_PIDTBSDINFO flavor.
        let n = unsafe {
            libc::proc_pidinfo(pid, libc::PROC_PIDTBSDINFO, 0, (&raw mut info).cast(), size)
        };
        if n != size {
            return None;
        }
        Some(info.pbi_start_tvsec * 1000 + info.pbi_start_tvusec / 1000)
    }
    #[cfg(not(any(target_os = "linux", target_os = "android", target_os = "macos")))]
    {
        let _ = pid;
        None
    }
}

/// Whether `o` is another ruse process still running on this host (RFC-0023 §7): same host name, its pid
/// answers `kill(pid, 0)`, and that process started when the Header says. A start time that cannot be read
/// counts as not running, so a reused pid or an unknown answer offers recovery rather than claim the file
/// is in use. This process is never "another".
pub fn is_live_other(o: &Owner) -> bool {
    if o == owner() || o.host != owner().host || !alive(o.pid) {
        return false;
    }
    process_start_ms(o.pid).is_some_and(|s| s.abs_diff(o.start_ms) <= START_SLACK_MS)
}

/// The owner recorded in the journal at `path`'s Header, when it has an intact one. Reads only a bounded
/// prefix: the Header comes first and is small.
pub fn read_owner(path: &Path) -> Option<Owner> {
    match read_head(path)? {
        Ok((rj01::Record::Header(h), _)) => Some(h.owner),
        _ => None,
    }
}

/// The first record of the journal at `path`, from a bounded prefix; `None` when it cannot be read.
pub(super) fn read_head(path: &Path) -> Option<Result<(rj01::Record, usize), rj01::Stop>> {
    let f = open_regular(path, OpenOptions::new().read(true)).ok()?;
    let mut raw = Vec::new();
    f.take(HEADER_MAX).read_to_end(&mut raw).ok()?;
    Some(rj01::read_record(&raw, 0))
}

/// The most of a journal [`read_owner`] reads: a Header holds a host name and a path.
const HEADER_MAX: u64 = 64 << 10;

/// Whether `pid` is a running process (`kill(pid, 0)`). An unknown answer counts as alive.
pub(crate) fn alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        let Ok(pid) = libc::pid_t::try_from(pid) else {
            return false;
        };
        // SAFETY: signal 0 checks for existence and permission only; nothing is delivered.
        if unsafe { libc::kill(pid, 0) } == 0 {
            return true;
        }
        io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        true
    }
}
