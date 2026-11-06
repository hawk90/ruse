//! Runtime health check (F-030 / CAP-HEALTHCHECK) — the `:checkhealth` report. A PURE builder (a plain
//! frontend snapshot in -> a report out), so it is unit-testable without a terminal; `:checkhealth` is a
//! thin wiring over it in `main.rs`. The Neovim `:checkhealth` analogue: an OK / WARN / absent list a user
//! reads to diagnose "why isn't X working" without the ntic log.

/// One check's outcome — the Neovim checkhealth shape. `Absent` is "feature not present yet" (not an error).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Ok,
    Warn,
    Absent,
}

impl Status {
    fn glyph(self) -> &'static str {
        match self {
            Status::Ok => "OK",
            Status::Warn => "WARN",
            Status::Absent => "n/a",
        }
    }
}

/// One reported row: a label, its status, and a one-line human detail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HealthRow {
    pub label: String,
    pub status: Status,
    pub detail: String,
}

/// A plain snapshot of the running frontend — deliberately carries NO frontend types, so the builder is
/// decoupled and fully testable; `main.rs` maps its live state into this.
pub struct HealthInputs {
    /// Active input profile, e.g. `"vim"` / `"emacs"`.
    pub profile: &'static str,
    /// Caret gravity, e.g. `"on-char"` / `"between-char"`.
    pub caret: &'static str,
    pub sgr_mouse: bool,
    pub sync_output: bool,
    pub bracketed_paste: bool,
    /// Focus reporting (mode 1004): a focus gain looks at the files on disk again (#1220).
    pub focus_events: bool,
    /// The focused buffer's file extension (without the dot), or `None` for a nameless/extension-less buffer.
    pub file_ext: Option<String>,
    /// Whether a tree-sitter grammar is available for that extension.
    pub grammar_ok: bool,
    pub buffers: usize,
    /// Number of commands recorded on the replay trace so far.
    pub trace_commands: usize,
    /// The recovery journals (RFC-0023 §8, #938).
    pub journaling: Journaling,
    /// What is wrong with the config, `None` when every layer loaded cleanly
    /// ([`crate::config::health_issue`]: `safe-mode (…)` or `degraded (…)`).
    pub config: Option<String>,
}

/// Where the session's recovery journals stand, for the journal rows.
pub enum Journaling {
    /// This session does not journal (a headless session).
    Off,
    /// Journaling is on but there is no state directory to write to.
    NoStateDir,
    /// The journal directory and one entry per file buffer.
    On {
        dir: String,
        /// The archive of discarded journals (RFC-0023 §9).
        archive: crate::persist::retention::ArchiveStats,
        buffers: Vec<JournalInput>,
    },
}

/// One file buffer's journal.
pub struct JournalInput {
    /// The buffer's file, as the session shows it.
    pub file: String,
    pub modified: bool,
    /// Its writer's health; `None` before the buffer's first frame.
    pub health: Option<crate::persist::journal::Health>,
}

fn journal_rows(j: &Journaling) -> Vec<HealthRow> {
    match j {
        Journaling::Off => vec![row(
            "recovery journal",
            Status::Absent,
            "not journaled in this session".into(),
        )],
        Journaling::NoStateDir => vec![row(
            "recovery journal",
            Status::Warn,
            "no state directory (set XDG_STATE_HOME or HOME): unsaved work is not journaled".into(),
        )],
        Journaling::On {
            dir,
            archive,
            buffers,
        } => {
            let mut rows = vec![row("recovery journal", Status::Ok, format!("in {dir}"))];
            rows.push(archive_row(archive));
            rows.extend(buffers.iter().map(journal_row));
            rows
        }
    }
}

/// The archive's size and when it was last swept (RFC-0023 §9, #934).
fn archive_row(a: &crate::persist::retention::ArchiveStats) -> HealthRow {
    let files = if a.count == 1 { "file" } else { "files" };
    let swept = a.last_sweep_ms.map_or_else(
        || "not swept yet".to_string(),
        |ms| format!("last swept {}", crate::persist::journal::date_time(ms)),
    );
    row(
        "journal archive",
        Status::Ok,
        format!("{} {files}, {} bytes, {swept}", a.count, a.bytes),
    )
}

fn journal_row(b: &JournalInput) -> HealthRow {
    let label = format!("journal: {}", b.file);
    let Some(h) = b.health.as_ref() else {
        return row(&label, Status::Ok, "no journal yet".into());
    };
    let failures = match h.failures {
        0 => String::new(),
        1 => ", 1 failed write".into(),
        n => format!(", {n} failed writes"),
    };
    if let Some(e) = &h.failing {
        return row(
            &label,
            Status::Warn,
            format!("last write failed: {e}{failures}, {}", h.journal.display()),
        );
    }
    if !b.modified && h.size == 0 {
        return row(&label, Status::Ok, format!("clean, no journal{failures}"));
    }
    let synced = h.synced_ago.map_or_else(
        || "not synced yet".to_string(),
        |d| format!("synced {}s ago", d.as_secs()),
    );
    let waiting = if h.unsynced { ", changes waiting" } else { "" };
    row(
        &label,
        Status::Ok,
        format!(
            "{} bytes, {synced}{waiting}{failures}, {}",
            h.size,
            h.journal.display()
        ),
    )
}

fn row(label: &str, status: Status, detail: String) -> HealthRow {
    HealthRow {
        label: label.into(),
        status,
        detail,
    }
}

/// Build the full report — one row per check.
pub fn report(i: &HealthInputs) -> Vec<HealthRow> {
    let cap = |name: &str, on: bool| {
        row(
            name,
            if on { Status::Ok } else { Status::Warn },
            if on {
                "enabled".into()
            } else {
                "not detected — degraded fallback".into()
            },
        )
    };
    vec![
        row(
            "input profile",
            Status::Ok,
            format!("{} (caret: {})", i.profile, i.caret),
        ),
        cap("term: sgr-mouse", i.sgr_mouse),
        cap("term: synchronized-output", i.sync_output),
        cap("term: bracketed-paste", i.bracketed_paste),
        cap("term: focus-events", i.focus_events),
        match &i.file_ext {
            Some(ext) if i.grammar_ok => row(
                "syntax grammar",
                Status::Ok,
                format!("tree-sitter grammar for .{ext}"),
            ),
            Some(ext) => row(
                "syntax grammar",
                Status::Absent,
                format!("no grammar for .{ext}"),
            ),
            None => row(
                "syntax grammar",
                Status::Absent,
                "buffer has no file extension".into(),
            ),
        },
        row("buffers", Status::Ok, format!("{} open", i.buffers)),
        row(
            "trace / replay",
            Status::Ok,
            format!("recording ({} commands)", i.trace_commands),
        ),
    ]
    .into_iter()
    .chain(journal_rows(&i.journaling))
    .chain(std::iter::once(match &i.config {
        None => row("config", Status::Ok, "ok".into()),
        Some(issue) => row("config", Status::Warn, issue.clone()),
    }))
    .collect()
}

/// The report as the lines `:checkhealth` lists, the summary last: one `OK` / `WARN` / `n/a` line per check.
pub fn lines(rows: &[HealthRow]) -> Vec<String> {
    rows.iter()
        .map(|r| format!("{:<4} {}: {}", r.status.glyph(), r.label, r.detail))
        .chain(std::iter::once(summary_line(rows)))
        .collect()
}

/// A compact one-line summary for the status area, under the listing [`lines`] opens: the OK count plus
/// every non-OK check spelled out, so the actionable items are visible at a glance.
pub fn summary_line(rows: &[HealthRow]) -> String {
    let ok = rows.iter().filter(|r| r.status == Status::Ok).count();
    let issues: Vec<String> = rows
        .iter()
        .filter(|r| r.status != Status::Ok)
        .map(|r| format!("{} {}", r.label, r.status.glyph()))
        .collect();
    if issues.is_empty() {
        format!("checkhealth: all {ok} ok")
    } else {
        format!(
            "checkhealth: {ok} ok, {} issue(s) — {}",
            issues.len(),
            issues.join(", ")
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> HealthInputs {
        HealthInputs {
            profile: "emacs",
            caret: "between-char",
            sgr_mouse: true,
            sync_output: true,
            bracketed_paste: true,
            focus_events: true,
            file_ext: Some("rs".into()),
            grammar_ok: true,
            buffers: 2,
            trace_commands: 7,
            journaling: Journaling::Off,
            config: None,
        }
    }

    #[test]
    fn report_covers_every_check_and_all_ok_reads_clean() {
        let rows = report(&sample());
        // profile, 4 term caps, grammar, buffers, trace, journal, config = 10 rows.
        assert_eq!(rows.len(), 10);
        assert!(rows[..8].iter().all(|r| r.status == Status::Ok));
        assert_eq!(rows[8].status, Status::Absent, "a headless session");
        assert_eq!(rows[9].label, "config");
        assert_eq!(rows[9].status, Status::Ok);
        assert_eq!(
            summary_line(&rows),
            "checkhealth: 9 ok, 1 issue(s) — recovery journal n/a"
        );
    }

    /// The config row (`config-model.md` Observability): `ok`, else safe mode or degraded is a WARN.
    #[test]
    fn the_config_row_reports_safe_mode() {
        let mut i = sample();
        i.config = Some("safe-mode (--safe-mode)".into());
        let l = lines(&report(&i));
        assert!(
            l.contains(&"WARN config: safe-mode (--safe-mode)".to_string()),
            "{l:?}"
        );
        assert!(l.last().unwrap().contains("config WARN"));
    }

    #[test]
    fn warnings_and_absent_surface_in_the_summary() {
        let mut i = sample();
        i.sgr_mouse = false; // -> WARN
        i.file_ext = Some("xyz".into());
        i.grammar_ok = false; // -> Absent
        let rows = report(&i);
        let s = summary_line(&rows);
        assert!(s.starts_with("checkhealth: 7 ok, 3 issue(s)"), "{s}");
        assert!(s.contains("term: sgr-mouse WARN"), "{s}");
        assert!(s.contains("syntax grammar n/a"), "{s}");
    }

    #[test]
    fn no_extension_reports_grammar_absent() {
        let mut i = sample();
        i.file_ext = None;
        let rows = report(&i);
        let g = rows.iter().find(|r| r.label == "syntax grammar").unwrap();
        assert_eq!(g.status, Status::Absent);
        assert_eq!(g.detail, "buffer has no file extension");
    }

    /// RFC-0023 §8 (#938): each file buffer's journal path, size, last sync age and failures.
    #[test]
    fn journal_rows_report_each_buffer() {
        use crate::persist::journal::Health;
        use std::path::PathBuf;
        use std::time::Duration;
        let h = |size, failing: Option<&str>, failures| Health {
            journal: PathBuf::from("/s/f.txt.0123456789abcdef.rj"),
            size,
            unsynced: false,
            synced_ago: Some(Duration::from_secs(3)),
            failures,
            failing: failing.map(str::to_string),
        };
        let mut i = sample();
        i.journaling = Journaling::On {
            dir: "/s".into(),
            archive: crate::persist::retention::ArchiveStats {
                count: 3,
                bytes: 4096,
                last_sweep_ms: None,
            },
            buffers: vec![
                JournalInput {
                    file: "f.txt".into(),
                    modified: true,
                    health: Some(h(120, None, 1)),
                },
                JournalInput {
                    file: "g.txt".into(),
                    modified: true,
                    health: Some(h(0, Some("Permission denied (os error 13)"), 2)),
                },
                JournalInput {
                    file: "h.txt".into(),
                    modified: false,
                    health: Some(h(0, None, 0)),
                },
            ],
        };
        let l = lines(&report(&i));
        assert_eq!(l[8], "OK   recovery journal: in /s");
        assert_eq!(
            l[9],
            "OK   journal archive: 3 files, 4096 bytes, not swept yet"
        );
        assert_eq!(
            l[10],
            "OK   journal: f.txt: 120 bytes, synced 3s ago, 1 failed write, /s/f.txt.0123456789abcdef.rj"
        );
        assert_eq!(
            l[11],
            "WARN journal: g.txt: last write failed: Permission denied (os error 13), 2 failed \
             writes, /s/f.txt.0123456789abcdef.rj"
        );
        assert_eq!(l[12], "OK   journal: h.txt: clean, no journal");
        assert_eq!(
            l.last().unwrap(),
            "checkhealth: 13 ok, 1 issue(s) — journal: g.txt WARN"
        );
        i.journaling = Journaling::NoStateDir;
        assert!(summary_line(&report(&i)).contains("recovery journal WARN"));
    }
}
