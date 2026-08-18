//! The command palette overlay (F-004 #2): a context-filtered, query-narrowed list of commands that
//! Enter dispatches by stable id through the normal command path. A [`Picker`]`<Command>` — the accept
//! action (run the command) lives at the call site in `app::session`.

use ruse_core::Workspace;

use crate::input::InputEngine;
use crate::ui::picker::{PickItem, Picker};

/// Open the command palette for `ctx`: a [`Picker`] over the commands available in that context, each row
/// searchable by title+id and displayed with its static keymap binding (resolved once here via `engine`,
/// since bindings don't change while the overlay is open) and category. Enter's payload is the [`Command`]
/// the caller dispatches.
pub(crate) fn open(ctx: &ruse_core::Context, engine: &InputEngine) -> Picker<ruse_core::Command> {
    let items = ruse_core::available(ctx)
        .into_iter()
        .map(|s| {
            let binding = engine
                .binding_label(&s.command)
                .unwrap_or_else(|| "—".into());
            PickItem {
                search: format!("{} {}", s.title, s.id),
                display: format!("{:<28} {:>5}   {:?}", s.title, binding, s.category),
                payload: s.command,
            }
        })
        .collect();
    Picker::new(items)
}

/// The command-availability context of the focused view (F-004 #2 C-CONTEXT).
pub(crate) fn focused_context(ws: &Workspace) -> ruse_core::Context {
    let f = ws.focused();
    let bytes = f.doc.bytes();
    let has_selection =
        f.view.selection_span(bytes).is_some() || f.view.block_spans(bytes).is_some();
    ruse_core::Context {
        mode: f.view.mode(),
        has_selection,
    }
}

#[cfg(test)]
mod palette_tests {
    use super::*;
    use ruse_core::{Command, Context, Mode};

    fn normal_ctx() -> Context {
        Context {
            mode: Mode::Normal,
            has_selection: false,
        }
    }

    /// F-004 #2 (C-CONTEXT): the palette's source is context-filtered — a Normal-family command is
    /// offered, an Insert-only command is hidden. (Context filtering lives in `ruse_core::available`.)
    #[test]
    fn palette_source_is_context_filtered() {
        let ids: Vec<_> = ruse_core::available(&normal_ctx())
            .iter()
            .map(|s| s.id)
            .collect();
        assert!(
            ids.contains(&"editor.undo"),
            "Normal-family command offered"
        );
        assert!(
            !ids.contains(&"editor.delete_back"),
            "Insert-only command hidden in Normal"
        );
    }

    /// F-004 #2: the query narrows the picker; Enter's payload is the selected command (by id, decoupled
    /// from any key). An empty query keeps the full available set.
    #[test]
    fn palette_query_narrows_to_command() {
        let engine = InputEngine::new();
        let full = open(&normal_ctx(), &engine);
        assert_eq!(
            full.rows().len(),
            ruse_core::available(&normal_ctx()).len(),
            "empty query keeps all available"
        );
        assert!(!full.rows().is_empty());

        let mut q = open(&normal_ctx(), &engine);
        for c in "save".chars() {
            q.on_key(crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Char(c),
                crossterm::event::KeyModifiers::NONE,
            ));
        }
        assert_eq!(
            q.rows().len(),
            1,
            "query narrows to the single 'Save File' match"
        );
        assert_eq!(q.selected(), Some(&Command::Save));
    }
}
