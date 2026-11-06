//! Crash-loop detection for safe mode (CFG-SAFE-001, `config-model.md` Failure modes).
//!
//! A start is laid at the config's door when it read a user or workspace layer and then ended before its
//! first frame was drawn: a panic, a signal or an abort between loading the config and painting. That
//! window is where a config value first takes effect (options, keymaps, the profile), and nothing else
//! runs in it that depends on the user. A crash later in the session is not counted: the user was editing,
//! and the config already worked once.
//!
//! The count of such starts in a row lives in one small file under the state directory
//! (`${XDG_STATE_HOME:-~/.local/state}/ruse/config-boot`), so it survives the crash it counts:
//!
//! - a start that reads a user or workspace layer writes `n + 1` before it applies the config ([`BootRecord::begin`]);
//! - its first frame removes the file ([`BootRecord::clear`]): the config booted;
//! - a start that finds `n >= CRASH_LIMIT` runs in safe mode and leaves the file as it is, so safe mode
//!   holds until the user leaves it on purpose (`:config-reload`, which clears the file once the full
//!   config applied). There is no silent auto-heal (`config-model.md` Recovery behavior).
//!
//! Several sessions at once share the file; a session that draws its first frame clears it for all of
//! them, which can only delay an escalation, never cause a false one.

use std::path::PathBuf;

use super::SafeMode;

/// The starts in a row that end before their first frame after which the next one runs in safe mode.
/// Three, as Firefox's `toolkit.startup.max_resumed_crashes`: one crash can be bad luck, a second can be
/// the user trying again at once, a third is a loop.
pub const CRASH_LIMIT: u32 = 3;

/// The crash-loop counter file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BootRecord {
    path: PathBuf,
}

impl BootRecord {
    /// The record under the state directory (`XDG_STATE_HOME`, else `~/.local/state`), or `None` with
    /// neither.
    #[must_use]
    pub fn from_env() -> Option<BootRecord> {
        let var = |n: &str| {
            std::env::var_os(n)
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
        };
        let state =
            var("XDG_STATE_HOME").or_else(|| var("HOME").map(|h| h.join(".local/state")))?;
        Some(BootRecord::at(state.join("ruse").join("config-boot")))
    }

    #[must_use]
    pub fn at(path: PathBuf) -> BootRecord {
        BootRecord { path }
    }

    #[cfg(test)]
    fn path(&self) -> &std::path::Path {
        &self.path
    }

    /// The starts in a row that ended before their first frame: the file's number, 0 when it is absent
    /// or holds anything else (a damaged record must not lock a user into safe mode).
    #[must_use]
    pub fn failed(&self) -> u32 {
        let Ok(text) = std::fs::read_to_string(&self.path) else {
            return 0;
        };
        text.trim().parse().unwrap_or(0)
    }

    /// Record one more start that has not drawn its first frame yet, `failed + 1` (write-new-then-rename,
    /// so a crash mid-write leaves the old count).
    ///
    /// # Errors
    /// The state directory or the file cannot be written.
    pub fn begin(&self, failed: u32) -> std::io::Result<()> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = self.path.with_extension("tmp");
        std::fs::write(&tmp, format!("{}\n", failed.saturating_add(1)))?;
        std::fs::rename(&tmp, &self.path)
    }

    /// The config booted: forget the count.
    pub fn clear(&self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// The safe mode a start runs in: asked for with `--safe-mode`, or after [`CRASH_LIMIT`] failed starts.
#[must_use]
pub fn decide(asked: bool, failed: u32) -> Option<SafeMode> {
    if asked {
        Some(SafeMode::Asked)
    } else if failed >= CRASH_LIMIT {
        Some(SafeMode::CrashLoop { failed })
    } else {
        None
    }
}

/// What a start does with the record: `Some(record)` while the record must be cleared at the first frame.
/// In safe mode the record stays (sticky); otherwise a start that read a user or workspace layer counts
/// itself ([`BootRecord::begin`]), and any start with a count clears it once it draws.
#[must_use]
pub fn arm(
    record: Option<BootRecord>,
    safe: Option<SafeMode>,
    reads_user: bool,
) -> Option<BootRecord> {
    let record = record?;
    if safe.is_some() {
        return None;
    }
    let failed = record.failed();
    let begun = reads_user && record.begin(failed).is_ok();
    (begun || failed > 0).then_some(record)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(name: &str) -> BootRecord {
        let dir = std::env::temp_dir().join(format!("ruse-boot-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        BootRecord::at(dir.join("ruse").join("config-boot"))
    }

    /// One start: decide from the record, arm it, and either crash (`drawn == false`, the record stays)
    /// or draw the first frame (the armed record is cleared). Returns the safe mode it ran in.
    fn start(r: &BootRecord, asked: bool, reads_user: bool, drawn: bool) -> Option<SafeMode> {
        let safe = decide(asked, r.failed());
        let armed = arm(Some(r.clone()), safe, reads_user && safe.is_none());
        if drawn {
            if let Some(a) = armed {
                a.clear();
            }
        }
        safe
    }

    #[test]
    fn three_crashed_starts_escalate_and_safe_mode_holds() {
        let r = record("loop");
        for n in 1..=CRASH_LIMIT {
            assert_eq!(start(&r, false, true, false), None, "start {n} is normal");
            assert_eq!(r.failed(), n);
        }
        let loop_mode = Some(SafeMode::CrashLoop {
            failed: CRASH_LIMIT,
        });
        // The next start is safe, draws, and still leaves the record: no silent way out.
        assert_eq!(start(&r, false, true, true), loop_mode);
        assert_eq!(start(&r, false, true, true), loop_mode);
        assert_eq!(r.failed(), CRASH_LIMIT);
        // `:config-reload` applied the full config: the record goes, the next start is normal.
        r.clear();
        assert_eq!(start(&r, false, true, true), None);
        assert_eq!(r.failed(), 0);
        let _ = std::fs::remove_dir_all(r.path().parent().unwrap().parent().unwrap());
    }

    #[test]
    fn a_start_that_draws_resets_the_count() {
        let r = record("reset");
        start(&r, false, true, false);
        start(&r, false, true, false);
        assert_eq!(r.failed(), 2);
        assert_eq!(start(&r, false, true, true), None);
        assert_eq!(r.failed(), 0, "the first frame cleared it");
        let _ = std::fs::remove_dir_all(r.path().parent().unwrap().parent().unwrap());
    }

    #[test]
    fn only_a_start_that_read_user_config_counts() {
        let r = record("nouser");
        // No user or workspace layer: a crash is not the config's, nothing is written.
        start(&r, false, false, false);
        assert_eq!(r.failed(), 0);
        assert!(!r.path().exists());
        // A count left by earlier starts is still cleared by a start without user config that draws.
        start(&r, false, true, false);
        start(&r, false, false, true);
        assert_eq!(r.failed(), 0);
        let _ = std::fs::remove_dir_all(r.path().parent().unwrap().parent().unwrap());
    }

    #[test]
    fn asked_safe_mode_neither_counts_nor_clears() {
        let r = record("asked");
        start(&r, false, true, false);
        assert_eq!(start(&r, true, true, false), Some(SafeMode::Asked));
        assert_eq!(start(&r, true, true, true), Some(SafeMode::Asked));
        assert_eq!(r.failed(), 1, "--safe-mode leaves the count alone");
        let _ = std::fs::remove_dir_all(r.path().parent().unwrap().parent().unwrap());
    }

    #[test]
    fn a_damaged_record_reads_as_zero() {
        let r = record("damaged");
        std::fs::create_dir_all(r.path().parent().unwrap()).unwrap();
        std::fs::write(r.path(), "lots\n").unwrap();
        assert_eq!(r.failed(), 0);
        assert_eq!(decide(false, r.failed()), None);
        let _ = std::fs::remove_dir_all(r.path().parent().unwrap().parent().unwrap());
    }
}
