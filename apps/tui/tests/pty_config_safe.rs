//! Safe mode and live reload of the config (#1152, CFG-SAFE-001) with the real binary. The harness points
//! `XDG_CONFIG_HOME` and `XDG_STATE_HOME` at the scratch `home`, so the user layer is
//! `home/ruse/config.toml` and the crash-loop record is `home/ruse/config-boot`. Shift width is read back
//! from the file `>>` writes.

#![cfg(unix)]

mod support;

use std::path::Path;

use support::{Editor, Scratch};

const ESC: &str = "\u{1b}";

fn user_config(dir: &Scratch, text: &str) {
    std::fs::create_dir_all(dir.path("home/ruse")).unwrap();
    std::fs::write(dir.path("home/ruse/config.toml"), text).unwrap();
}

/// Shift the first line once more, write, and return the file.
fn shift_and_read(ed: &mut Editor, file: &Path) -> String {
    ed.keys(&format!("{ESC}gg0>>:w\r"));
    ed.wait_for("written");
    String::from_utf8(std::fs::read(file).unwrap()).unwrap()
}

#[test]
fn safe_mode_flag_ignores_the_user_config() {
    let dir = Scratch::new();
    user_config(&dir, "editor.shift_width = 2\n");
    let file = dir.file("f.txt", b"x\n");
    let mut ed = Editor::start(&dir, &[Path::new("--safe-mode"), Path::new("f.txt")]);
    ed.wait_for("Config: safe-mode (--safe-mode)");
    assert_eq!(
        shift_and_read(&mut ed, &file),
        "    x\n",
        "ruse's default 4"
    );
    // The flag stays on the status line, and `:inspect config` names the layers it did not read.
    ed.wait_for_row("[Config: safe-mode]");
    ed.keys(":inspect config\r");
    ed.wait_for("user and workspace layers not read");
    ed.keys(&format!("{ESC}:q\r"));
    assert_eq!(ed.exit_status(), 0);
    assert!(
        !dir.path("home/ruse/config-boot").exists(),
        "--safe-mode does not count a start"
    );
}

/// The record says three starts in a row died before their first frame (what three crashed starts
/// leave): the next start is in safe mode and keeps the record until `:config-reload` loads the full
/// config again.
#[test]
fn a_crash_loop_escalates_to_safe_mode_until_config_reload() {
    let dir = Scratch::new();
    user_config(&dir, "editor.shift_width = 2\n");
    std::fs::write(dir.path("home/ruse/config-boot"), "3\n").unwrap();
    let file = dir.file("f.txt", b"x\n");
    let mut ed = Editor::start(&dir, &[Path::new("f.txt")]);
    ed.wait_for("Config: safe-mode (after 3 crashed starts)");
    assert_eq!(shift_and_read(&mut ed, &file), "    x\n");
    assert_eq!(
        std::fs::read_to_string(dir.path("home/ruse/config-boot")).unwrap(),
        "3\n",
        "safe mode holds until the user leaves it"
    );
    ed.keys(":config-reload\r");
    ed.wait_for("safe mode off");
    assert_eq!(
        shift_and_read(&mut ed, &file),
        "      x\n",
        "4 + the user's 2"
    );
    assert!(!dir.path("home/ruse/config-boot").exists());
    ed.keys(":q\r");
    assert_eq!(ed.exit_status(), 0);
}

/// A start that reads user config counts itself, and its first frame clears the count; a start below
/// the limit is not safe.
#[test]
fn a_start_that_draws_clears_the_count() {
    let dir = Scratch::new();
    user_config(&dir, "editor.shift_width = 2\n");
    std::fs::create_dir_all(dir.path("home/ruse")).unwrap();
    std::fs::write(dir.path("home/ruse/config-boot"), "2\n").unwrap();
    let file = dir.file("f.txt", b"x\n");
    let mut ed = Editor::start(&dir, &[Path::new("f.txt")]);
    assert_eq!(
        shift_and_read(&mut ed, &file),
        "  x\n",
        "not safe: the user's 2"
    );
    assert!(!dir.path("home/ruse/config-boot").exists());
    ed.keys(":q\r");
    assert_eq!(ed.exit_status(), 0);
}

/// Three real crashes: each start loads the user config and aborts before its first frame (the test
/// hook `RUSE_TEST_CRASH_BEFORE_FIRST_FRAME`), and the fourth start, without the hook, is in safe mode.
#[test]
fn three_crashed_starts_escalate_to_safe_mode() {
    let dir = Scratch::new();
    user_config(&dir, "editor.shift_width = 2\n");
    let file = dir.file("f.txt", b"x\n");
    for n in 1..=3 {
        let mut ed = Editor::start_env(
            &dir,
            &[Path::new("f.txt")],
            &["RUSE_TEST_CRASH_BEFORE_FIRST_FRAME=1"],
        );
        assert_eq!(ed.exit_status(), -libc::SIGABRT, "start {n} crashed");
        assert_eq!(
            std::fs::read_to_string(dir.path("home/ruse/config-boot")).unwrap(),
            format!("{n}\n")
        );
    }
    let mut ed = Editor::start(&dir, &[Path::new("f.txt")]);
    ed.wait_for("Config: safe-mode (after 3 crashed starts): no user or workspace config");
    assert_eq!(shift_and_read(&mut ed, &file), "    x\n");
    ed.keys(":q\r");
    assert_eq!(ed.exit_status(), 0);
}

/// A crash without user config is not the config's: nothing is counted.
#[test]
fn a_crash_without_user_config_is_not_counted() {
    let dir = Scratch::new();
    dir.file("f.txt", b"x\n");
    let mut ed = Editor::start_env(
        &dir,
        &[Path::new("f.txt")],
        &["RUSE_TEST_CRASH_BEFORE_FIRST_FRAME=1"],
    );
    assert_eq!(ed.exit_status(), -libc::SIGABRT);
    assert!(!dir.path("home/ruse/config-boot").exists());
}

#[test]
fn a_changed_user_config_reloads_and_a_broken_one_keeps_the_last() {
    let dir = Scratch::new();
    user_config(&dir, "editor.shift_width = 2\n");
    let file = dir.file("f.txt", b"x\n");
    let mut ed = Editor::start(&dir, &[Path::new("f.txt")]);
    assert_eq!(shift_and_read(&mut ed, &file), "  x\n");
    // `:checktime` looks at the files now instead of after the idle wait.
    user_config(&dir, "# three\neditor.shift_width = 3\n");
    ed.keys(":checktime\r");
    ed.wait_for("config reloaded: 1 key(s) changed (editor.shift_width)");
    assert_eq!(shift_and_read(&mut ed, &file), "     x\n", "2 + 3");
    // A file that no longer parses keeps the previous config and says so.
    user_config(&dir, "[editor\nshift_width = 7\n");
    ed.keys(":checktime\r");
    // The message is longer than the line: it waits for ENTER, as nvim's does.
    ed.wait_for("config not reloaded, the previous one stays");
    ed.keys("\r");
    assert_eq!(shift_and_read(&mut ed, &file), "        x\n", "still 3");
    // A typed `:set` wins over a reload; `input.profile` waits for the next start.
    ed.keys(":set sw=1\r");
    user_config(&dir, "editor.shift_width = 6\ninput.profile = \"emacs\"\n");
    ed.keys(":config-reload\r");
    ed.wait_for("(editor.shift_width); at the next start: input");
    ed.keys("\r");
    assert_eq!(
        shift_and_read(&mut ed, &file),
        "         x\n",
        "the typed 1"
    );
    ed.keys(":q\r");
    assert_eq!(ed.exit_status(), 0);
}

/// Without a change on disk, nothing is said; `:config-reload` always answers.
#[test]
fn config_reload_reports_no_change() {
    let dir = Scratch::new();
    user_config(&dir, "editor.shift_width = 2\n");
    dir.file("f.txt", b"x\n");
    let mut ed = Editor::start(&dir, &[Path::new("f.txt")]);
    ed.keys(":config-reload\r");
    ed.wait_for("config reloaded: no change");
    ed.keys(":q\r");
    assert_eq!(ed.exit_status(), 0);
}
