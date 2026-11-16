//! Wall-clock rendering for the recovery prompts. Split from `journal.rs` (#1389).

/// Local wall-clock `HH:MM:SS` of Unix milliseconds `ms`.
pub fn clock(ms: u64) -> String {
    let (_, time) = local(ms);
    time
}

/// Local `YYYY-MM-DD HH:MM:SS` of Unix milliseconds `ms`.
pub fn date_time(ms: u64) -> String {
    let (date, time) = local(ms);
    format!("{date} {time}")
}

/// The local date and time of `ms`; UTC (and marked so) when the local time cannot be had.
fn local(ms: u64) -> (String, String) {
    #[cfg(unix)]
    {
        let t = (ms / 1000) as libc::time_t;
        // SAFETY: `tm` is plain data, fully written by localtime_r on success.
        let mut tm: libc::tm = unsafe { std::mem::zeroed() };
        // SAFETY: both pointers are valid for the call.
        if !unsafe { libc::localtime_r(&t, &mut tm) }.is_null() {
            return (
                format!(
                    "{:04}-{:02}-{:02}",
                    tm.tm_year + 1900,
                    tm.tm_mon + 1,
                    tm.tm_mday
                ),
                format!("{:02}:{:02}:{:02}", tm.tm_hour, tm.tm_min, tm.tm_sec),
            );
        }
    }
    // Days since the epoch to a civil date (Howard Hinnant's algorithm).
    let days = (ms / 86_400_000) as i64;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    let s = ms / 1000 % 86_400;
    (
        format!("{y:04}-{m:02}-{d:02}"),
        format!("{:02}:{:02}:{:02} UTC", s / 3600, s / 60 % 60, s % 60),
    )
}
