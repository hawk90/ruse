//! The one modal picker overlay slot (F-004 / F-013 NAT-3, #879): every special-view picker is a
//! [`Picker<T>`] over its own payload, and [`Overlay`] holds whichever one is open. One slot makes "at most
//! one is open" a type fact: opening a picker replaces the open one, so the overlay that is drawn is the
//! one that gets the keys.

use std::path::PathBuf;

use crossterm::event::KeyEvent;
use ruse_core::{Command, DocumentId};

use crate::lsp::protocol::CodeAction;
use crate::ui::picker::{PickOutcome, Picker};

/// The open picker overlay, by kind.
pub(crate) enum Overlay {
    /// The command palette (`C-p`); Enter dispatches the selected command.
    Palette(Picker<Command>),
    /// The buffer-line jump (`C-l`); payload is the line's byte offset.
    Lines(Picker<usize>),
    /// The buffer switch (Native `C-b`).
    Buffers(Picker<DocumentId>),
    /// The file finder (Native `C-f`); Enter opens the path like `:e`.
    Files(Picker<PathBuf>),
    /// F-014: LSP references — `(uri, line, character)`; Enter jumps (same or other file).
    Refs(Picker<(String, u32, u32)>),
    /// F-014: LSP code actions; Enter applies the action's WorkspaceEdit.
    Actions(Picker<CodeAction>),
    /// F-014: the `:diagnostics` list; payload is the diagnostic's start byte.
    Diagnostics(Picker<usize>),
    /// F-029: the `:registers` viewer; view-only.
    Registers(Picker<char>),
    /// The `:digraphs` listing; view-only.
    Digraphs(Picker<char>),
    /// The `[I`/`]I` keyword lines and multi-row print output (`:p`, `:z`, `:g`); view-only.
    Listing(Picker<usize>),
    /// F-003: the `:marks` viewer; payload is the mark's byte offset.
    Marks(Picker<usize>),
    /// F-003: the `:jumps` / `:changes` position viewer; payload is a byte offset.
    Positions(Picker<usize>),
}

/// What an accepted row asks the session to do.
pub(crate) enum Accepted {
    /// Move the focused cursor to this byte offset.
    Jump(usize),
    /// Focus this buffer in the focused window.
    Buffer(DocumentId),
    /// Open this file (the interactive `:e`).
    File(PathBuf),
    /// Jump to an LSP location.
    Location(String, u32, u32),
    /// Apply an LSP code action.
    Action(CodeAction),
    /// Dispatch a command as if typed.
    Command(Command),
}

/// Run `$body` with `$p` bound to the open overlay's picker, whatever its payload type.
macro_rules! each_picker {
    ($ov:expr, $p:ident => $body:expr) => {
        match $ov {
            Overlay::Palette($p) => $body,
            Overlay::Lines($p)
            | Overlay::Diagnostics($p)
            | Overlay::Listing($p)
            | Overlay::Marks($p)
            | Overlay::Positions($p) => $body,
            Overlay::Buffers($p) => $body,
            Overlay::Files($p) => $body,
            Overlay::Refs($p) => $body,
            Overlay::Actions($p) => $body,
            Overlay::Registers($p) | Overlay::Digraphs($p) => $body,
        }
    };
}

impl Overlay {
    /// The match rows (`display`, selected flag) for the above-the-status-line paint slot.
    pub(crate) fn rows(&self) -> Vec<(String, bool)> {
        each_picker!(self, p => p.rows())
    }

    /// The command-line prompt: a glyph naming the overlay, and the typed query.
    pub(crate) fn prompt(&self) -> (char, &str) {
        let glyph = match self {
            Overlay::Palette(_) => '>',
            Overlay::Lines(_) => '#', // line jump
            Overlay::Buffers(_) => '@',
            Overlay::Files(_) => '~',   // find file
            Overlay::Refs(_) => '*',    // references to the symbol
            Overlay::Actions(_) => '!', // actions / fixes at the cursor
            Overlay::Diagnostics(_) => '✗',
            Overlay::Registers(_) => '"',
            Overlay::Digraphs(_) => '§',
            Overlay::Listing(_) => 'i', // `[I`/`]I` identifiers
            Overlay::Marks(_) => '\'',
            Overlay::Positions(_) => '↕', // jumps / changes
        };
        (glyph, each_picker!(self, p => p.query.as_str()))
    }

    /// Feed one key to the picker: its outcome, and on Enter what the selected row asks for (`None` for a
    /// view-only listing or when nothing matches).
    pub(crate) fn on_key(&mut self, key: KeyEvent) -> (PickOutcome, Option<Accepted>) {
        let outcome = each_picker!(self, p => p.on_key(key));
        if !matches!(outcome, PickOutcome::Accept) {
            return (outcome, None);
        }
        let accepted = match self {
            Overlay::Palette(p) => p.selected().cloned().map(Accepted::Command),
            Overlay::Lines(p)
            | Overlay::Diagnostics(p)
            | Overlay::Marks(p)
            | Overlay::Positions(p) => p.selected().copied().map(Accepted::Jump),
            Overlay::Buffers(p) => p.selected().copied().map(Accepted::Buffer),
            Overlay::Files(p) => p.selected().cloned().map(Accepted::File),
            Overlay::Refs(p) => p
                .selected()
                .cloned()
                .map(|(uri, l, c)| Accepted::Location(uri, l, c)),
            Overlay::Actions(p) => p.selected().cloned().map(Accepted::Action),
            Overlay::Registers(_) | Overlay::Digraphs(_) | Overlay::Listing(_) => None,
        };
        (outcome, accepted)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::picker::PickItem;
    use crossterm::event::{KeyCode, KeyModifiers};

    fn picker<T>(payloads: Vec<T>) -> Picker<T> {
        Picker::new(
            payloads
                .into_iter()
                .enumerate()
                .map(|(i, payload)| PickItem {
                    search: format!("row{i}"),
                    display: format!("row{i}"),
                    payload,
                })
                .collect(),
        )
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn enter_on_a_jump_picker_yields_its_offset_and_a_listing_yields_nothing() {
        let mut lines = Overlay::Lines(picker(vec![3, 9]));
        lines.on_key(key(KeyCode::Down));
        let (outcome, accepted) = lines.on_key(key(KeyCode::Enter));
        assert!(matches!(outcome, PickOutcome::Accept));
        assert!(matches!(accepted, Some(Accepted::Jump(9))));

        let mut listing = Overlay::Listing(picker(vec![1, 2]));
        let (outcome, accepted) = listing.on_key(key(KeyCode::Enter));
        assert!(matches!(outcome, PickOutcome::Accept));
        assert!(accepted.is_none(), "a view-only listing accepts nothing");
    }

    #[test]
    fn typing_filters_and_the_prompt_shows_the_query() {
        let mut marks = Overlay::Marks(picker(vec![0, 5, 7]));
        let (outcome, accepted) = marks.on_key(key(KeyCode::Char('2')));
        assert!(matches!(outcome, PickOutcome::Continue));
        assert!(accepted.is_none());
        assert_eq!(marks.prompt(), ('\'', "2"));
        assert_eq!(marks.rows(), vec![("row2".to_string(), true)]);
        let (outcome, _) = marks.on_key(key(KeyCode::Esc));
        assert!(matches!(outcome, PickOutcome::Cancel));
    }
}
