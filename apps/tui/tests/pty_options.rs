//! The #1151 pieces only the real binary shows: `'showmode'` on the status line, and the user config's
//! `keymap.lang` installed into the Lang-Arg map at startup.

#![cfg(unix)]

mod support;

use support::{Editor, Scratch};

const ESC: &str = "\u{1b}";

/// The bytes the editor wrote after the first `from` bytes.
fn since(ed: &Editor, from: usize) -> String {
    ed.screen_bytes()[from..].to_string()
}

#[test]
fn noshowmode_leaves_the_mode_off_the_status_line() {
    let dir = Scratch::new();
    dir.file("f.txt", b"abc\n");
    let mut ed = Editor::start(&dir, &[std::path::Path::new("f.txt")]);
    ed.keys(":set nosmd\r");
    let mark = ed.screen_bytes().len();
    ed.keys("i");
    assert!(
        !since(&ed, mark).contains("INSERT"),
        "'noshowmode' still names the mode"
    );
    ed.keys(&format!("{ESC}:set smd\r"));
    let mark = ed.screen_bytes().len();
    ed.keys("i");
    assert!(
        since(&ed, mark).contains("INSERT"),
        "'showmode' names the mode"
    );
    ed.keys(&format!("{ESC}:q!\r"));
    assert_eq!(ed.exit_status(), 0);
}

#[test]
fn verbose_above_zero_names_where_an_option_was_set() {
    // nvim: with 'verbose' at 1 or more, `:set sw?` adds the `Last set from` line `:verbose` gives.
    let dir = Scratch::new();
    dir.file("f.txt", b"x\n");
    std::fs::create_dir_all(dir.path("home/ruse")).unwrap();
    dir.file("home/ruse/config.toml", b"[editor]\nshift_width = 2\n");
    let mut ed = Editor::start(&dir, &[std::path::Path::new("f.txt")]);
    let mark = ed.screen_bytes().len();
    ed.keys(":set sw?\r");
    assert!(!since(&ed, mark).contains("Last set from"));
    ed.keys(":set vbs=1\r");
    let mark = ed.screen_bytes().len();
    ed.keys(":set sw?\r");
    assert!(
        since(&ed, mark).contains("Last set from"),
        "{}",
        since(&ed, mark)
    );
    ed.keys(&format!("{ESC}:q\r"));
    assert_eq!(ed.exit_status(), 0);
}

#[test]
fn the_user_config_keymap_lang_is_installed() {
    let dir = Scratch::new();
    dir.file("f.txt", b"\n");
    // The harness points XDG_CONFIG_HOME at `home`.
    std::fs::create_dir_all(dir.path("home/ruse")).unwrap();
    dir.file(
        "home/ruse/config.toml",
        b"[keymap.lang]\na = \"x\"\nbad = \"y\"\n",
    );
    let mut ed = Editor::start(&dir, &[std::path::Path::new("f.txt")]);
    assert!(
        ed.screen_bytes().contains("keymap.lang"),
        "the skipped entry is reported"
    );
    // CTRL-^ in Insert turns the language map on; `a` then types `x`.
    ed.keys(&format!("i\u{1e}ab{ESC}:wq\r"));
    assert_eq!(ed.exit_status(), 0);
    assert_eq!(std::fs::read(dir.path("f.txt")).unwrap(), b"xb\n");
}
