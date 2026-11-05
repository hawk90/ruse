//! The config loader end to end (#614): the real binary reads the user layer
//! (`$XDG_CONFIG_HOME/ruse/config.toml`) and the workspace layer (`.ruse/config.toml` in its working
//! directory), and the workspace layer loads only after `:trust` recorded its content hash under
//! `$XDG_STATE_HOME/ruse/trust/`. The harness points both XDG directories at the scratch `home`. Results
//! are read from disk (`>>` writes the configured shiftwidth) and from the screen.

#![cfg(unix)]

mod support;

use std::path::Path;

use support::{Editor, Scratch};

const ESC: &str = "\u{1b}";

/// Open `f.txt` (`x`), shift it once, write and quit; return the file.
fn shifted(dir: &Scratch, before: &str) -> String {
    let file = dir.file("f.txt", b"x\n");
    let mut ed = Editor::start(dir, &[Path::new("f.txt")]);
    ed.keys(before);
    ed.keys(&format!("{ESC}>>:wq\r"));
    assert_eq!(ed.exit_status(), 0);
    String::from_utf8(std::fs::read(file).unwrap()).unwrap()
}

fn user_config(dir: &Scratch, text: &str) {
    std::fs::create_dir_all(dir.path("home/ruse")).unwrap();
    std::fs::write(dir.path("home/ruse/config.toml"), text).unwrap();
}

fn workspace_config(dir: &Scratch, text: &str) {
    std::fs::create_dir_all(dir.path(".ruse")).unwrap();
    std::fs::write(dir.path(".ruse/config.toml"), text).unwrap();
}

#[test]
fn user_config_sets_options_and_a_bad_key_only_warns() {
    let dir = Scratch::new();
    user_config(
        &dir,
        "[editor]\nshift_width = 2\nindent_style = \"space\"\ntabwidth = 9\n",
    );
    // The good keys apply even though the file has an unknown one.
    assert_eq!(shifted(&dir, ""), "  x\n");
    // The unknown key is reported once, at startup, and `:verbose set` names the file and line.
    let mut ed = Editor::start(&dir, &[Path::new("f.txt")]);
    ed.wait_for("unknown key editor.tabwidth");
    // `~/ruse/…` or `home/ruse/…`: the scratch HOME lies inside the working directory.
    let shown = ed.keys_then_wait_for(
        ":verbose set sw?\r",
        "ruse/config.toml line 2 (user config)",
    );
    assert!(shown.contains("Last set from"), "{shown}");
    ed.keys(&format!("{ESC}:q\r"));
    assert_eq!(ed.exit_status(), 0);
}

#[test]
fn a_malformed_user_config_still_starts() {
    let dir = Scratch::new();
    user_config(&dir, "[editor\nshift_width = 2\n");
    assert_eq!(shifted(&dir, ""), "    x\n");
}

#[test]
fn workspace_config_loads_only_while_its_trust_record_matches() {
    let dir = Scratch::new();
    user_config(&dir, "editor.shift_width = 2\n");
    workspace_config(&dir, "[editor]\nshift_width = 3\n");
    // Untrusted: the workspace layer is inert, the user's value holds.
    assert_eq!(shifted(&dir, ""), "  x\n");
    // `:trust` records the hash and loads the layer at once.
    assert_eq!(shifted(&dir, ":trust\r"), "   x\n");
    let records = std::fs::read_dir(dir.path("home/ruse/trust"))
        .unwrap()
        .count();
    assert_eq!(
        records, 1,
        "one user-side record, nothing in the repository"
    );
    assert!(!dir.path(".ruse/trust").exists());
    // The next start is trusted.
    assert_eq!(shifted(&dir, ""), "   x\n");
    // A change under `.ruse/` (a pull, a branch switch) makes it stale: inert until `:trust` again.
    workspace_config(&dir, "[editor]\nshift_width = 5\n");
    assert_eq!(shifted(&dir, ""), "  x\n");
    assert_eq!(shifted(&dir, ":trust\r"), "     x\n");
    // `:untrust` deletes the record and unloads the layer.
    assert_eq!(shifted(&dir, ":untrust\r"), "  x\n");
    assert_eq!(shifted(&dir, ""), "  x\n");
}

#[test]
fn a_workspace_cannot_set_user_or_locked_keys() {
    let dir = Scratch::new();
    workspace_config(
        &dir,
        "[editor]\nscrolloff = 0\nshift_width = 3\n[keymap.normal]\nx = \"dd\"\n",
    );
    let file = dir.file("f.txt", b"x\n");
    let mut ed = Editor::start(&dir, &[Path::new("f.txt")]);
    ed.keys(":trust\r");
    // The workspace's `x` binding never applies: `x` still deletes one character.
    ed.keys(&format!("x:inspect config\r{ESC}:wq\r"));
    assert_eq!(ed.exit_status(), 0);
    assert_eq!(std::fs::read(file).unwrap(), b"\n");
}
