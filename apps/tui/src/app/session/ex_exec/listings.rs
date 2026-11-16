//! The Ex commands that only show state (#1500): `:checkhealth`, `:inspect config`, and the
//! `:marks`, `:diagnostics` and `:registers` pickers.

use crate::app::session::disk_watch::journal_health;
// The pickers come through the session's own imports; this module is their only user.
use crate::app::session::{diag_picker, marks_picker, register_picker, Editing, FrameCtx, Session};
use crate::health;
use crate::ui::overlay::Overlay;

impl Session {
    /// `:checkhealth` (F-030): gather the frontend snapshot (guard/profile/highlighter) and render
    /// the report's one-line summary into status.
    pub(in crate::app::session) fn ex_checkhealth(&mut self) {
        let Session {
            ed:
                Editing {
                    ws,
                    files,
                    recorded,
                    status,
                    ..
                },
            highlighters,
            profile_name,
            emacs_profile,
            term,
            config,
            overlay,
            rescue,
            journals,
            journal_writers,
            ..
        } = self;
        let inputs = health::HealthInputs {
            profile: profile_name,
            caret: if *emacs_profile {
                "between-char"
            } else {
                "on-char"
            },
            sgr_mouse: term.sgr_mouse,
            sync_output: term.sync_output,
            bracketed_paste: term.bracketed_paste,
            focus_events: term.focus_events,
            file_ext: files
                .get(&ws.focused_buffer())
                .and_then(|bf| bf.path.extension())
                .map(|e| e.to_string_lossy().into_owned()),
            grammar_ok: highlighters.contains_key(&ws.focused_buffer()),
            buffers: ws.window_count(),
            trace_commands: recorded.len(),
            journaling: journal_health(*rescue, *journals, files, ws, journal_writers),
            config: crate::config::health_issue(config),
        };
        // Every check's line in the listing, the summary on the status line (RFC-0023 §8).
        crate::app::ex_more::show_rows(health::lines(&health::report(&inputs)), status, overlay);
    }

    /// `:inspect config [key]` (CFG-PROV-001): the layers, their state, every value a layer set
    /// with its source, and the diagnostics; or one key's layer stack.
    pub(in crate::app::session) fn ex_inspect_config(&mut self, key: Option<String>) {
        let Session {
            config,
            overlay,
            ed: Editing { status, .. },
            ..
        } = self;
        let rows = match key.as_deref() {
            None => Some(crate::config::inspect_all(config)),
            Some(k) => crate::config::inspect_key(config, k),
        };
        match rows {
            Some(rows) => crate::app::ex_more::show_rows(rows, status, overlay),
            None => {
                status.error(format!("unknown config key: {}", key.unwrap_or_default()));
            }
        }
    }

    /// `:marks` (F-003): open a picker over the set marks; Enter jumps to the selected one.
    pub(in crate::app::session) fn ex_marks(&mut self) {
        let Session {
            ed: Editing { ws, status, .. },
            overlay,
            ..
        } = self;
        let snapshot = ws.marks_snapshot();
        if snapshot.is_empty() {
            status.echo("no marks set");
        } else {
            status.echo(format!("{} mark(s)", snapshot.len()));
            *overlay = Some(Overlay::Marks(marks_picker::open(
                snapshot.into_iter().map(|(c, p)| (c, p.0)).collect(),
                ws.focused().doc.bytes(),
            )));
        }
    }

    /// `:diagnostics` (F-014): open a picker over the focused buffer's already-collected
    /// diagnostics (no server round-trip); Enter jumps to the selected one.
    pub(in crate::app::session) fn ex_diagnostics(&mut self, frame: &FrameCtx) {
        let Session {
            ed: Editing { ws, status, .. },
            overlay,
            lsp,
            ..
        } = self;
        let snapshot = &frame.snapshot;
        let diags = lsp.diagnostics_for(ws.focused_buffer());
        if diags.is_empty() {
            status.echo("no diagnostics");
        } else {
            status.echo(format!("{} diagnostic(s)", diags.len()));
            *overlay = Some(Overlay::Diagnostics(diag_picker::open(diags, snapshot)));
        }
    }

    /// `:registers` (F-029): open a view-only picker over the non-empty registers, so a
    /// recorded macro (`"a`) or a yank is inspectable.
    pub(in crate::app::session) fn ex_registers(&mut self) {
        let Session {
            ed: Editing { ws, status, .. },
            overlay,
            ..
        } = self;
        let snapshot = ws.register_snapshot();
        if snapshot.is_empty() {
            status.echo("no registers set");
        } else {
            status.echo(format!("{} register(s)", snapshot.len()));
            *overlay = Some(Overlay::Registers(register_picker::open(snapshot)));
        }
    }
}
