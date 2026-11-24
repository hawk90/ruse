use crate::input::*;

/// The palette label of every registered command available in `mode`, under `e`'s profile.
fn labels(e: &InputEngine, mode: Mode) -> Vec<(&'static str, Option<String>)> {
    let specs = ruse_core::available(&ruse_core::Context {
        mode,
        has_selection: false,
    });
    let cmds: Vec<Command> = specs.iter().map(|s| s.command.clone()).collect();
    let out = e.binding_labels(&cmds, mode);
    specs.iter().map(|s| s.id).zip(out).collect()
}

fn label<'a>(all: &'a [(&'static str, Option<String>)], id: &str) -> Option<&'a str> {
    all.iter()
        .find(|(i, _)| *i == id)
        .unwrap_or_else(|| panic!("{id} not offered"))
        .1
        .as_deref()
}

/// F-004 #2 (#956): the palette shows the key that runs each command in the current mode under the
/// active profile — grammar keys included, not only the static layer tables.
#[test]
fn binding_labels_follow_the_grammar_and_the_profile() {
    let vim = InputEngine::new();
    let normal = labels(&vim, Mode::Normal);
    for (id, key) in [
        ("editor.insert", "i"),
        ("editor.insert_after", "a"),
        ("editor.undo", "u"),
        ("editor.redo", "C-r"),
        ("editor.undo_older", "g-"),
        ("editor.undo_newer", "g+"),
        ("editor.join_lines", "J"),
        ("editor.line_start", "0"),
        ("editor.line_end", "$"),
        ("editor.reselect", "gv"),
    ] {
        assert_eq!(label(&normal, id), Some(key), "{id}");
    }
    // Vim has no key for `:w` or `:q` (`ZZ` is `:x`, `ZQ` is `:q!`).
    assert_eq!(label(&normal, "file.save"), None);
    assert_eq!(label(&normal, "file.quit"), None);
    let insert = labels(&vim, Mode::Insert);
    assert_eq!(label(&insert, "editor.normal_mode"), Some("Esc"));
    assert_eq!(label(&insert, "editor.newline"), Some("CR"));
    assert_eq!(label(&insert, "editor.delete_back"), Some("BS"));
    assert_eq!(label(&insert, "editor.break_undo"), Some("C-g u"));
    // The Emacs profile's own keys, not Vim's.
    let emacs = labels(&InputEngine::emacs(), Mode::Insert);
    assert_eq!(label(&emacs, "file.save"), Some("C-x C-s"));
    assert_eq!(label(&emacs, "file.quit"), Some("C-x C-c"));
    assert_eq!(label(&emacs, "editor.break_undo"), None);
}

/// Arm an operator prefix, then resolve a viewport screen-motion (`H`/`M`/`L`) to the ABSOLUTE `line`
/// the frontend computed — exercises `screen_op`, the operator-composition seam for `dH`/`yL`/`>H`/… .
fn op_to_line(prefix: &str, line: u32) -> Feed {
    let mut e = InputEngine::new();
    for c in prefix.chars() {
        e.feed(
            KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE),
            Mode::Normal,
        );
    }
    let count = e.screen_count();
    e.screen_op(ScreenMotion::ViewHigh, count, line)
}

/// The OPERATOR forms of `H`/`M`/`L` (`:help H`): any pending operator composes with the viewport-resolved
/// target line over `GotoLine` (LINEWISE in core). Ground truth verified against nvim v0.12.4: e.g. `dH`
/// from a mid-screen line deletes up THROUGH the top visible line inclusive. The frontend hands the
/// resolved line in; here we assert the command `screen_op` builds for each operator.
#[test]
fn screen_motions_compose_with_operators() {
    // No operator armed: plain `H`/`M`/`L` stays a bare cursor move (preserved behavior).
    assert_eq!(
        op_to_line("", 4),
        Feed::Cmd(Command::Move(4, Motion::GotoLine))
    );
    // `dH` / `cM` / `yL` — delete/change/yank LINEWISE through the resolved line. Direction is handled by
    // the core `GotoLine` operator range (`[min,max]` of cursor and target), so the same command serves
    // cursor-above-target and cursor-below-target.
    assert_eq!(
        op_to_line("d", 4),
        Feed::Cmd(Command::Delete(4, Motion::GotoLine))
    );
    assert_eq!(
        op_to_line("c", 5),
        Feed::Cmd(Command::Change(5, Motion::GotoLine))
    );
    assert_eq!(
        op_to_line("y", 1),
        Feed::Cmd(Command::Yank(1, Motion::GotoLine))
    );
    // `>H` / `<L` / `=M` — shift and reindent compose too (always linewise).
    assert_eq!(
        op_to_line(">", 3),
        Feed::Cmd(Command::ShiftMotion {
            left: false,
            count: 3,
            motion: Motion::GotoLine,
        })
    );
    assert_eq!(
        op_to_line("<", 8),
        Feed::Cmd(Command::ShiftMotion {
            left: true,
            count: 8,
            motion: Motion::GotoLine,
        })
    );
    assert_eq!(
        op_to_line("=", 7),
        Feed::Cmd(Command::Reindent {
            count: 7,
            motion: Motion::GotoLine,
        })
    );
    // `gUH` / `guL` / `g~M` — the case operators compose as well.
    assert_eq!(
        op_to_line("gU", 2),
        Feed::Cmd(Command::CaseMotion {
            count: 2,
            motion: Motion::GotoLine,
            case: WordCase::Upcase,
            wise: None,
        })
    );
    assert_eq!(
        op_to_line("gu", 2),
        Feed::Cmd(Command::CaseMotion {
            count: 2,
            motion: Motion::GotoLine,
            case: WordCase::Downcase,
            wise: None,
        })
    );
    assert_eq!(
        op_to_line("g~", 6),
        Feed::Cmd(Command::CaseMotion {
            count: 6,
            motion: Motion::GotoLine,
            case: WordCase::Toggle,
            wise: None,
        })
    );
}

/// The effective count the frontend uses to resolve the `H`/`M`/`L` TARGET LINE from the viewport, and
/// the pending-operator flag it uses to drop 'scrolloff' under an operator. Count multiplication matches
/// nvim: `2H` = 2, `3d2H` = 6 (op-count times motion-count), `3dH` = 3, bare `H`/`dH` = 0/1.
#[test]
fn screen_count_and_has_op_track_pending_state() {
    fn state(prefix: &str) -> (u32, bool) {
        let mut e = InputEngine::new();
        for c in prefix.chars() {
            e.feed(
                KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE),
                Mode::Normal,
            );
        }
        (e.screen_count(), e.has_op())
    }
    // No count, no operator: raw count 0 (viewport resolver treats it as line 1), no op → scrolloff kept.
    assert_eq!(state(""), (0, false));
    // Bare count (plain `2H`): count 2, no op.
    assert_eq!(state("2"), (2, false));
    // Operator only (`dH`): effective count 1, op armed → scrolloff dropped.
    assert_eq!(state("d"), (1, true));
    // Count after the operator (`d2H`): 2.
    assert_eq!(state("d2"), (2, true));
    // Count before the operator (`3dH`): 3.
    assert_eq!(state("3d"), (3, true));
    // Both multiply (`3d2H` = 6H): 6.
    assert_eq!(state("3d2"), (6, true));
}
