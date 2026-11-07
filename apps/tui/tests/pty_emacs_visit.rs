//! The Emacs profile's visiting variables from the user config on a real PTY (#619), against Emacs 31.1
//! (`files.el`): `find-sibling-rules`, `confirm-nonexistent-file-or-buffer` t, `directory-abbrev-alist` and
//! `find-file-visit-truename`.

#![cfg(unix)]

mod support;

use std::path::Path;

use support::{Editor, Scratch};

fn emacs(dir: &Scratch, file: &str, config: &str) -> Editor {
    std::fs::create_dir_all(dir.path("home/ruse")).unwrap();
    std::fs::write(
        dir.path("home/ruse/config.toml"),
        format!("[emacs]\n{config}"),
    )
    .unwrap();
    let mut ed = Editor::start_env(dir, &[Path::new(file)], &["RUSE_PROFILE=emacs"]);
    ed.wait_for_row(file);
    ed
}

fn quit(mut ed: Editor) {
    ed.keys("\x18\x03");
    assert_eq!(ed.exit_status(), 0);
}

#[test]
fn find_sibling_file_follows_the_rules() {
    let dir = Scratch::new();
    dir.file("a.src", b"int main;\n");
    dir.file("a.hdr", b"extern int main;\n");
    let mut ed = emacs(
        &dir,
        "a.src",
        "[emacs.find_sibling_rules]\n'\\([^/]\\+\\)\\.src$' = '\\1.hdr'\n",
    );
    ed.keys("\x1bxfind-sibling-file\r");
    ed.wait_for_row("extern int main;");
    quit(ed);
}

#[test]
fn confirm_nonexistent_file_asks_for_a_second_return() {
    let dir = Scratch::new();
    dir.file("f.txt", b"one\n");
    let mut ed = emacs(
        &dir,
        "f.txt",
        "confirm_nonexistent_file_or_buffer = \"t\"\n",
    );
    ed.keys("\x18\x06");
    ed.wait_for_row("ile: /");
    ed.keys("new.txt\r");
    ed.wait_for_row("new.txt [Confirm]");
    ed.keys("\r");
    ed.wait_for_row("(New file)");
    quit(ed);
}

#[test]
fn directory_abbrev_alist_and_truename_shape_the_name() {
    let dir = Scratch::new();
    dir.file("f.txt", b"one\n");
    dir.file("g.txt", b"two\n");
    std::os::unix::fs::symlink("g.txt", dir.path("link.txt")).unwrap();
    let real = std::fs::canonicalize(&dir.0).unwrap();
    let mut ed = emacs(
        &dir,
        "f.txt",
        &format!(
            "find_file_visit_truename = true\n[emacs.directory_abbrev_alist]\n'^/nonexistent-ruse-abbrev' = '{}'\n",
            real.display()
        ),
    );
    // The abbreviation turns a name under a directory that does not exist into the scratch directory.
    ed.keys("\x18\x06");
    ed.wait_for_row("ile: /");
    let back = "\x7f".repeat(400);
    ed.keys(&format!("{back}/nonexistent-ruse-abbrev/link.txt\r"));
    // The true name of the link is g.txt.
    let rows = ed.wait_for_row("two");
    assert!(!rows.iter().any(|r| r.contains("link.txt")), "{rows:?}");
    quit(ed);
}
