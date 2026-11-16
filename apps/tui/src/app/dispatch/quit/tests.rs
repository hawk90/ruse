use ruse_core::{Command, SplitDir};

use super::*;
use crate::app::dispatch::tests::ex_run;

/// `:q` refuses (E37) once the focused buffer is modified; `:q!` quits regardless; a clean `:q` quits.
#[test]
fn quit_guards_unsaved_changes_and_bang_overrides() {
    let run = |line: &str, ws: &mut Workspace| ex_run(ws, line);

    // Clean buffer → `:q` quits.
    let mut ws = Workspace::new(b"hello\n".to_vec());
    let (_, quit) = run("q", &mut ws);
    assert!(quit, "clean buffer quits on :q");

    // Modify the buffer → `:q` refuses with E37, `:q!` quits.
    let mut ws = Workspace::new(b"hello\n".to_vec());
    ws.apply(&Command::EnterInsert);
    ws.apply(&Command::InsertChar('x'));
    assert!(ws.focused().doc.is_modified(), "edit dirtied the buffer");
    let (status, quit) = run("q", &mut ws);
    assert!(!quit, ":q does not quit a dirty buffer");
    assert!(status.starts_with("E37"), "E37 surfaced: {status}");
    let (_, quit) = run("q!", &mut ws);
    assert!(quit, ":q! discards changes and quits");
}

/// A workspace with a.txt focused and a MODIFIED b.txt loaded but hidden (`:split`, `:e b.txt`, `x`,
/// `:close` in nvim).
fn ws_with_hidden_modified() -> Workspace {
    let mut ws = Workspace::new(b"a\n".to_vec());
    ws.set_focused_buffer_name("a.txt");
    let b = ws.add_buffer(b"b\n".to_vec(), Some("b.txt".into()));
    ws.split(SplitDir::Horizontal);
    ws.focus_buffer(b);
    ws.apply(&Command::DeleteUnder(1));
    assert!(ws.close_focused());
    assert!(!ws.focused().doc.is_modified());
    ws
}

/// #611, verified against nvim v0.12.6: `:q` / `:q!` in a split close only the focused window, even
/// when its buffer is modified (that buffer stays loaded, hidden); `:close`/`:hide` refuse the last
/// window with E444.
#[test]
fn quit_in_a_split_closes_the_window_only() {
    for line in ["q", "quit", "q!"] {
        let mut ws = Workspace::new(b"a\n".to_vec());
        ws.split(SplitDir::Horizontal);
        ws.apply(&Command::DeleteUnder(1));
        let (status, quit) = ex_run(&mut ws, line);
        assert!(!quit, ":{line} in a split must not end the editor");
        assert_eq!(ws.window_count(), 1, ":{line} closes the focused window");
        assert!(ws.focused().doc.is_modified(), "the edit survives");
        assert_eq!(status, "");
    }
    for line in ["close", "close!", "hide"] {
        let mut ws = Workspace::new(b"a\n".to_vec());
        let (status, quit) = ex_run(&mut ws, line);
        assert!(!quit);
        assert_eq!(status, "E444: Cannot close last window", ":{line}");
    }
}

/// #611: on the last window, every quit that does not say "all" refuses while a HIDDEN buffer is
/// modified — `:q`, `:q!`, `:wq`, `:x` (nvim reports E37 for each; ruse names the buffer, E162).
/// `:qa` refuses too; only `:qa!` abandons it.
#[test]
fn last_window_quits_refuse_over_a_modified_hidden_buffer() {
    for line in ["q", "q!", "x", "qa", "qall", "quitall"] {
        let mut ws = ws_with_hidden_modified();
        let (status, quit) = ex_run(&mut ws, line);
        assert!(!quit, ":{line} must not drop b.txt's change");
        assert_eq!(
            status, "E162: No write since last change for buffer \"b.txt\"",
            ":{line}"
        );
    }
    let mut ws = ws_with_hidden_modified();
    assert!(ex_run(&mut ws, "qa!").1, ":qa! abandons every change");
}

/// `:qa` names the FOCUSED buffer first (E37) when it is the modified one.
#[test]
fn quit_all_refuses_a_modified_focused_buffer_with_e37() {
    let mut ws = Workspace::new(b"a\n".to_vec());
    ws.apply(&Command::DeleteUnder(1));
    let (status, quit) = ex_run(&mut ws, "qa");
    assert!(!quit);
    assert!(status.starts_with("E37"), "{status}");
    assert!(ex_run(&mut ws, "quitall!").1);
}

/// `:x` (and `ZZ`) writes only when modified: a clean unnamed buffer quits without a write attempt,
/// while a modified one with no file name fails to write and stays open (#842).
#[test]
fn exit_write_writes_only_a_modified_buffer() {
    let mut ws = Workspace::new(b"a\n".to_vec());
    assert!(ex_run(&mut ws, "x").1, "clean: quits, nothing to write");
    let mut ws = Workspace::new(b"a\n".to_vec());
    ws.apply(&Command::DeleteUnder(1));
    let (_, quit) = ex_run(&mut ws, "x");
    assert!(!quit, "a failed write keeps the editor open");
    let (_, quit) = ex_run(&mut ws, "wq");
    assert!(!quit, ":wq likewise");
}

/// `:cq [N]` quits at once, even over unsaved changes, and records the exit status (default 1).
#[test]
fn cquit_quits_with_a_status() {
    let mut ws = Workspace::new(b"a\n".to_vec());
    ws.apply(&Command::DeleteUnder(1));
    assert!(ex_run(&mut ws, "cq 3").1);
    assert_eq!(exit_status(), 3);
    assert!(ex_run(&mut ws, "cquit").1);
    assert_eq!(exit_status(), 1);
    EXIT_STATUS.store(0, Ordering::Relaxed);
}
