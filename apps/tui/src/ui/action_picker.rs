//! The LSP code-action picker overlay (F-014): list the actions (quickfixes/assists) the language server
//! offered at the cursor, filter by a typed title query, and on Enter apply the selected action's
//! `WorkspaceEdit`. A [`Picker`]`<CodeAction>` whose payload is the whole action; the accept action
//! (`LspCoordinator::apply_code_action`) lives in `app::session`, reusing the multi-file edit apply.

use crate::lsp::protocol::CodeAction;
use crate::ui::picker::{PickItem, Picker};

/// Open a code-action picker over the actions the server returned. Each row shows the action `title`; the
/// payload is the whole [`CodeAction`] so the caller can apply its edit.
pub(crate) fn open(actions: Vec<CodeAction>) -> Picker<CodeAction> {
    let items = actions
        .into_iter()
        .map(|a| PickItem {
            search: a.title.clone(),
            display: a.title.clone(),
            payload: a,
        })
        .collect();
    Picker::new(items)
}

#[cfg(test)]
mod action_picker_tests {
    use super::*;

    #[test]
    fn rows_show_titles_payload_keeps_edit() {
        let actions = vec![
            CodeAction {
                title: "Import Foo".into(),
                edit: vec![(
                    "file:///a.rs".into(),
                    vec![((0, 0), (0, 0), "use x;\n".into())],
                )],
            },
            CodeAction {
                title: "Add derive".into(),
                edit: vec![(
                    "file:///a.rs".into(),
                    vec![((1, 0), (1, 0), "#[derive(Debug)]\n".into())],
                )],
            },
        ];
        let p = open(actions);
        let rows: Vec<String> = p.rows().into_iter().map(|(r, _)| r).collect();
        assert_eq!(rows, vec!["Import Foo", "Add derive"]);
        assert_eq!(p.selected().map(|a| a.title.as_str()), Some("Import Foo"));
    }
}
