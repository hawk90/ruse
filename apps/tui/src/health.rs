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
    /// The focused buffer's file extension (without the dot), or `None` for a nameless/extension-less buffer.
    pub file_ext: Option<String>,
    /// Whether a tree-sitter grammar is available for that extension.
    pub grammar_ok: bool,
    pub buffers: usize,
    /// Number of commands recorded on the replay trace so far.
    pub trace_commands: usize,
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
}

/// A compact one-line summary for the status area (slice 1 has no scratch-buffer surface yet): the OK
/// count plus every non-OK check spelled out, so the actionable items are visible at a glance.
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
            file_ext: Some("rs".into()),
            grammar_ok: true,
            buffers: 2,
            trace_commands: 7,
        }
    }

    #[test]
    fn report_covers_every_check_and_all_ok_reads_clean() {
        let rows = report(&sample());
        // profile, 3 term caps, grammar, buffers, trace = 7 rows.
        assert_eq!(rows.len(), 7);
        assert!(rows.iter().all(|r| r.status == Status::Ok));
        assert_eq!(summary_line(&rows), "checkhealth: all 7 ok");
    }

    #[test]
    fn warnings_and_absent_surface_in_the_summary() {
        let mut i = sample();
        i.sgr_mouse = false; // -> WARN
        i.file_ext = Some("xyz".into());
        i.grammar_ok = false; // -> Absent
        let rows = report(&i);
        let s = summary_line(&rows);
        assert!(s.starts_with("checkhealth: 5 ok, 2 issue(s)"), "{s}");
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
}
