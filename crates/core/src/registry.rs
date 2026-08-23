//! C-COMMAND registry (F-004, `docs/design/command-engine.md`): named, namespaced commands invocable
//! by a STABLE id independent of any key binding (INV-CMD-SEMANTIC), plus the minimal C-CONTEXT
//! (`When`) that decides a command's availability — the one contract the palette (F-004 #2) and a
//! future `:command`/M-x surface both query.
//!
//! This is the MVP slice of the full [`CommandSpec`] in the design doc: it carries the fields the three
//! F-004 acceptance criteria need — a namespaced [`CommandSpec::id`], a human title + category, a typed
//! [`ArgSchema`] (introspectable, #3), and a [`When`] availability predicate (#2) — and DEFERS the
//! ecosystem fields (capabilities / trust / declared side-effects / undoable / exec-location /
//! repeatable / api-version) to post-MVP, when plugins and AI-review consume them. Every registered
//! command is a no-argument semantic [`Command`]; parameterised commands (motions, counts) stay
//! keybinding-driven and are not palette-invokable in the MVP.

use crate::command::Command;
use crate::editor::Mode;

/// A stable, namespaced command identity — the ecosystem ABI (D-006, INV-CMD-SEMANTIC). Rendered
/// dot-separated (`editor.undo`, `file.save`); it never changes once shipped and is independent of any
/// key binding. MVP uses `&'static str` (the full design interns an `Arc<str>`).
pub type CommandId = &'static str;

/// The palette grouping of a command (design §1 `CommandCategory`, MVP subset).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Category {
    /// Text-changing commands (insert, join, delete).
    Editing,
    /// Cursor movement.
    Navigation,
    /// Mode transitions (enter Insert / Normal).
    Mode,
    /// File operations (save, quit).
    File,
    /// Undo / redo / time-travel.
    History,
    /// Selection commands.
    Selection,
}

/// A command's typed argument schema (design §2). Introspectable for discovery/doc generation (F-004
/// #3). MVP registers only no-argument commands, so this is [`ArgSchema::None`]; the typed-argument
/// variants are the post-MVP elaboration.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ArgSchema {
    /// The command takes no arguments.
    None,
}

/// The editor context a [`When`] predicate is evaluated against (the MVP C-CONTEXT: mode + selection).
/// The full Context Key registry (view kind, buffer kind, focus, git state, …) is post-MVP.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Context {
    /// The active mode of the focused view.
    pub mode: Mode,
    /// Whether a Visual/Select selection is live.
    pub has_selection: bool,
}

/// A command availability predicate — the MVP `when`-expression (design §4). `matches` decides whether
/// the command is offered in a given [`Context`], which is what the palette filters on (F-004 #2).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum When {
    /// Always available.
    Always,
    /// Available only in exactly this mode (Normal or Insert; selection modes use `NormalFamily`).
    InMode(Mode),
    /// Available in the Normal-editing family (Normal / Visual / Select).
    NormalFamily,
    /// Available only while a selection is live.
    HasSelection,
}

impl When {
    /// Whether a command with this predicate is available in `ctx`.
    #[must_use]
    pub fn matches(self, ctx: &Context) -> bool {
        match self {
            When::Always => true,
            When::InMode(m) => ctx.mode == m,
            When::NormalFamily => {
                matches!(
                    ctx.mode,
                    Mode::Normal | Mode::Visual { .. } | Mode::Select { .. }
                )
            }
            When::HasSelection => ctx.has_selection,
        }
    }
}

/// A registered command (F-004 MVP `CommandSpec`): its stable id, human-facing title + category, typed
/// arg schema, availability predicate, and the no-arg semantic [`Command`] it dispatches to.
#[derive(Clone, Debug)]
pub struct CommandSpec {
    /// Stable, namespaced id — invoke the command by THIS, never by a key (#1).
    pub id: CommandId,
    /// Palette / menu label.
    pub title: &'static str,
    /// Palette grouping.
    pub category: Category,
    /// Typed argument schema (introspectable, #3).
    pub args: ArgSchema,
    /// Availability predicate (#2).
    pub when: When,
    /// The semantic command this id dispatches (INV-CMD-SEMANTIC): decoupled from any binding.
    pub command: Command,
}

/// A short constructor keeping the registry table readable.
fn spec(
    id: CommandId,
    title: &'static str,
    category: Category,
    when: When,
    command: Command,
) -> CommandSpec {
    CommandSpec {
        id,
        title,
        category,
        args: ArgSchema::None,
        when,
        command,
    }
}

/// The command registry — the single source of truth the palette and `command_by_id` query. Ids are
/// namespaced and stable (id stability is a test invariant). The set is the MVP's named, no-arg
/// commands across a spread of contexts; motions/operators stay keybinding-driven.
pub fn registry() -> Vec<CommandSpec> {
    use Category::{Editing, File, History, Mode as ModeCat, Navigation, Selection};
    vec![
        // Mode transitions.
        spec(
            "editor.insert",
            "Insert",
            ModeCat,
            When::NormalFamily,
            Command::EnterInsert,
        ),
        spec(
            "editor.insert_after",
            "Insert After Cursor",
            ModeCat,
            When::NormalFamily,
            Command::EnterInsertAfter,
        ),
        spec(
            "editor.normal_mode",
            "Normal Mode",
            ModeCat,
            When::Always,
            Command::EnterNormal,
        ),
        // History.
        spec(
            "editor.undo",
            "Undo",
            History,
            When::NormalFamily,
            Command::Undo,
        ),
        spec(
            "editor.redo",
            "Redo",
            History,
            When::NormalFamily,
            Command::Redo,
        ),
        spec(
            "editor.undo_older",
            "Undo — Older State",
            History,
            When::NormalFamily,
            Command::UndoOlder,
        ),
        spec(
            "editor.undo_newer",
            "Undo — Newer State",
            History,
            When::NormalFamily,
            Command::UndoNewer,
        ),
        // Editing.
        spec(
            "editor.join_lines",
            "Join Lines",
            Editing,
            When::NormalFamily,
            Command::JoinLines(1),
        ),
        spec(
            "editor.newline",
            "Insert Newline",
            Editing,
            When::InMode(Mode::Insert),
            Command::InsertNewline,
        ),
        spec(
            "editor.delete_back",
            "Delete Backward",
            Editing,
            When::InMode(Mode::Insert),
            Command::DeleteBack,
        ),
        spec(
            "editor.break_undo",
            "Break Undo Group",
            Editing,
            When::InMode(Mode::Insert),
            Command::BreakUndo,
        ),
        // Navigation.
        spec(
            "editor.line_start",
            "Go to Line Start",
            Navigation,
            When::NormalFamily,
            Command::MoveLineStart,
        ),
        spec(
            "editor.line_end",
            "Go to Line End",
            Navigation,
            When::NormalFamily,
            Command::MoveLineEnd,
        ),
        // Selection.
        spec(
            "editor.reselect",
            "Reselect Last Selection",
            Selection,
            When::InMode(Mode::Normal),
            Command::ReselectVisual,
        ),
        // File.
        spec("file.save", "Save File", File, When::Always, Command::Save),
        spec("file.quit", "Quit", File, When::Always, Command::Quit),
    ]
}

/// Look up a command by its stable id (F-004 #1: invocation is by id, not by key). `None` = no such id.
#[must_use]
pub fn command_by_id(id: &str) -> Option<CommandSpec> {
    registry().into_iter().find(|s| s.id == id)
}

/// The commands available in `ctx` (F-004 #2: the palette lists only context-appropriate commands).
#[must_use]
pub fn available(ctx: &Context) -> Vec<CommandSpec> {
    registry()
        .into_iter()
        .filter(|s| s.when.matches(ctx))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::SelectKind;

    fn normal() -> Context {
        Context {
            mode: Mode::Normal,
            has_selection: false,
        }
    }
    fn insert() -> Context {
        Context {
            mode: Mode::Insert,
            has_selection: false,
        }
    }

    /// F-004 #1: a command is invocable by its stable namespaced id, yielding the semantic command
    /// (decoupled from any key binding).
    #[test]
    fn invoke_by_stable_id() {
        assert_eq!(
            command_by_id("editor.undo").map(|s| s.command),
            Some(Command::Undo)
        );
        assert_eq!(
            command_by_id("file.save").map(|s| s.command),
            Some(Command::Save)
        );
        assert!(command_by_id("no.such.command").is_none());
    }

    /// F-004 #1/#3: ids are unique, non-empty, and namespaced (a dot-separated identity) — the ABI the
    /// palette and macros bind against.
    #[test]
    fn ids_are_unique_and_namespaced() {
        let all = registry();
        let mut ids: Vec<_> = all.iter().map(|s| s.id).collect();
        let n = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), n, "command ids must be unique");
        for s in &all {
            assert!(s.id.contains('.'), "{} must be namespaced", s.id);
            assert!(!s.title.is_empty());
        }
    }

    /// F-004 #2: availability is context-aware — Insert-only commands are hidden in Normal, and vice
    /// versa; `Always` commands appear in both.
    #[test]
    fn availability_is_context_filtered() {
        let in_normal: Vec<_> = available(&normal()).iter().map(|s| s.id).collect();
        assert!(
            in_normal.contains(&"editor.undo"),
            "undo is available in Normal"
        );
        assert!(in_normal.contains(&"file.save"), "save is always available");
        assert!(
            !in_normal.contains(&"editor.delete_back"),
            "Insert-only command is hidden in Normal"
        );

        let in_insert: Vec<_> = available(&insert()).iter().map(|s| s.id).collect();
        assert!(
            in_insert.contains(&"editor.delete_back"),
            "delete-back is available in Insert"
        );
        assert!(in_insert.contains(&"file.save"), "save is always available");
        assert!(
            !in_insert.contains(&"editor.undo"),
            "Normal-family command is hidden in Insert"
        );
    }

    /// F-004 #2: the Normal-editing FAMILY includes Visual/Select, so a Normal-family command is
    /// offered while a selection is live.
    #[test]
    fn normal_family_includes_visual() {
        let visual = Context {
            mode: Mode::Visual {
                kind: SelectKind::Charwise,
            },
            has_selection: true,
        };
        let ids: Vec<_> = available(&visual).iter().map(|s| s.id).collect();
        assert!(ids.contains(&"editor.undo"));
        assert!(
            !ids.contains(&"editor.reselect"),
            "reselect is Normal-only, not Visual"
        );
    }

    /// F-004 #3: metadata (typed arg schema + availability + category) is introspectable per command.
    #[test]
    fn metadata_is_introspectable() {
        let s = command_by_id("file.save").unwrap();
        assert_eq!(s.args, ArgSchema::None);
        assert_eq!(s.category, Category::File);
        assert_eq!(s.when, When::Always);
        assert_eq!(s.title, "Save File");
    }
}
