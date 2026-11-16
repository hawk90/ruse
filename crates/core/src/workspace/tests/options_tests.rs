//! Option levels and the workspace origin.

use super::*;
use crate::editor::{CaretGravity, SubFlags, SubRange};

/// `:setlocal` writes the focused View, `:setglobal` the copy new Views take, `:set` both; a
/// Vim-global option reaches every open View at any level (#614).
#[test]
fn set_option_levels() {
    use crate::editor::EditorOption;
    let ts = |opts: Vec<EditorOption>| {
        opts.into_iter()
            .find_map(|o| match o {
                EditorOption::TabStop(n) => Some(n),
                _ => None,
            })
            .unwrap()
    };
    let mut w = ws();
    w.set_option_in(EditorOption::TabStop(3), OptionLevel::Local);
    assert_eq!(ts(w.focused().view.editing_options()), 3);
    assert_eq!(ts(w.global_editing_options()), 4);
    w.set_option_in(EditorOption::TabStop(6), OptionLevel::Global);
    assert_eq!(ts(w.focused().view.editing_options()), 3);
    assert_eq!(ts(w.global_editing_options()), 6);
    let id = w.add_buffer(b"x".to_vec(), None);
    w.focus_buffer(id);
    assert_eq!(ts(w.focused().view.editing_options()), 6);
    // `ignorecase` is global: a `:setlocal` in one window reaches the other.
    w.split(SplitDir::Horizontal);
    w.set_option_in(EditorOption::IgnoreCase(true), OptionLevel::Local);
    w.focus_next();
    assert!(w
        .focused()
        .view
        .editing_options()
        .contains(&EditorOption::IgnoreCase(true)));
}

/// `:set ignorecase` flips the focused view's search case, observable through `:s` matching.
#[test]
fn set_option_ignorecase_changes_matching() {
    use crate::editor::EditorOption;
    let mut w = Workspace::new(b"Foo\n".to_vec());
    // Default is case-sensitive: `:s/foo/X/` matches nothing.
    let out = w
        .substitute(SubRange::CurrentLine, "foo", "X", SubFlags::default())
        .expect("valid regex");
    assert_eq!(out.replacements, 0);
    // `:set ignorecase` → now it matches.
    w.set_option(EditorOption::IgnoreCase(true));
    let out = w
        .substitute(SubRange::CurrentLine, "foo", "X", SubFlags::default())
        .expect("valid regex");
    assert_eq!(out.replacements, 1);
    assert_eq!(w.focused().doc.bytes(), b"X\n");
}

/// `set_default_search_case` applies to the initial buffer AND propagates to buffers created afterward,
/// so the shipped ignorecase+smartcase default holds across `:e` (not just the first buffer).
#[test]
fn default_search_case_propagates_to_new_buffers() {
    let mut w = Workspace::new(b"Foo\n".to_vec());
    w.set_default_search_case(true, true); // ruse's shipped default
                                           // The initial buffer is now case-insensitive.
    let out = w
        .substitute(SubRange::CurrentLine, "foo", "X", SubFlags::default())
        .expect("valid regex");
    assert_eq!(out.replacements, 1, "initial buffer honors the default");
    // A buffer opened AFTER the default was set also gets it.
    let b2 = w.add_buffer(b"Bar\n".to_vec(), Some("b2".to_string()));
    w.focus_buffer(b2);
    let out = w
        .substitute(SubRange::CurrentLine, "bar", "Y", SubFlags::default())
        .expect("valid regex");
    assert_eq!(out.replacements, 1, "new buffer inherits the default");
    // smartcase: an uppercase char in the pattern forces case-sensitivity even with ignorecase on.
    let b3 = w.add_buffer(b"baz Baz\n".to_vec(), Some("b3".to_string()));
    w.focus_buffer(b3);
    let out = w
        .substitute(SubRange::CurrentLine, "Baz", "Z", SubFlags::default())
        .expect("valid regex");
    assert_eq!(
        out.replacements, 1,
        "smartcase: 'Baz' matches only the capitalized one"
    );
    assert_eq!(w.focused().doc.bytes(), b"baz Z\n");
}

/// The workspace origin reaches every transaction a command makes, `U` included, so a macro replay's
/// edits are recorded as `Macro` (INV-ORIGIN, #880).
#[test]
fn commands_record_the_workspace_origin() {
    let mut w = Workspace::new(b"ab\ncd\n".to_vec());
    w.apply(&Command::Delete(1, Motion::Right));
    w.focused_doc_mut().sync_undo();
    w.set_origin(TransactionOrigin::Macro);
    w.apply(&Command::Delete(1, Motion::Right));
    w.focused_doc_mut().sync_undo();
    w.apply(&Command::LineUndo);
    w.set_origin(TransactionOrigin::UserInput);
    w.apply(&Command::Delete(1, Motion::Right));
    let origins: Vec<_> = w
        .focused()
        .doc
        .history()
        .creation_order()
        .into_iter()
        .filter_map(|(_, o, _)| o)
        .collect();
    assert_eq!(
        origins,
        [
            TransactionOrigin::UserInput,
            TransactionOrigin::Macro,
            TransactionOrigin::Macro,
            TransactionOrigin::UserInput,
        ]
    );
}

/// The caret gravity, the indent config and every `:set` reach buffers created afterward (Vim copies the
/// global option values into a new buffer; nvim 0.12.6 `:set ts=2 sw=3 noet tw=50 ic | enew` keeps all
/// five), including `:bd`'s replacement scratch (#754).
#[test]
fn options_and_caret_gravity_propagate_to_new_buffers() {
    use crate::editor::{EditorOption, IndentStyle};
    let mut w = Workspace::new(b"ab\n".to_vec());
    w.set_caret_gravity(CaretGravity::BetweenChar);
    w.set_indent(2, IndentStyle::Space);
    w.set_option(EditorOption::ShiftWidth(3));
    let b2 = w.add_buffer(b"cd\n".to_vec(), None);
    w.focus_buffer(b2);
    let v = w.focused().view;
    assert_eq!(v.caret_gravity(), CaretGravity::BetweenChar);
    assert_eq!((v.tab_width(), v.shift_width()), (2, 3));
    // A `:set` in one buffer sets the global value but leaves the other buffer's local value alone.
    w.set_option(EditorOption::TabStop(5));
    let b1 = w.buffers()[0].id;
    w.focus_buffer(b1);
    assert_eq!(
        w.focused().view.tab_width(),
        2,
        "b1 keeps its local tabstop"
    );
    assert!(w.remove_buffer(b1) && w.remove_buffer(b2));
    let v = w.focused().view;
    assert_eq!(v.caret_gravity(), CaretGravity::BetweenChar, "the scratch");
    assert_eq!((v.tab_width(), v.shift_width()), (5, 3));
}
