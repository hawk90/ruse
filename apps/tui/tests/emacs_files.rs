//! The Emacs profile's questions about files and buffers (#619), against Emacs 31.1 (`emacs -Q`, the
//! prompts read from `files.el` / `map-ynp.el` and confirmed in `--batch`): `C-x C-c` with
//! `save-some-buffers` and `Modified buffers exist; exit anyway?`, `C-x s`, `revert-buffer` and
//! `C-x x g`, `C-x C-q`, and `C-x C-f` of a file changed on disk.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use ruse_tui::headless::{Headless, Profile};

const SAVE_KEYS: &str = "(y, n, !, ., q, M-~, C-r, C-f, d or C-h) ";

/// A scratch directory with `f.txt` holding `text`.
fn file(text: &str) -> PathBuf {
    static N: AtomicU32 = AtomicU32::new(0);
    let d = std::env::temp_dir().join(format!(
        "ruse-emacs-files-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&d).unwrap();
    let f = std::path::absolute(d.join("f.txt")).unwrap();
    std::fs::write(&f, text).unwrap();
    f
}

fn emacs(f: &Path) -> Headless {
    Headless::visiting(Profile::Emacs, f)
}

fn read(f: &Path) -> String {
    std::fs::read_to_string(f).unwrap()
}

#[test]
fn kill_terminal_offers_to_save_then_exits() {
    let f = file("one\n");
    let mut h = emacs(&f);
    h.keys("x<C-x><C-c>");
    assert_eq!(
        h.status(),
        format!("Save file {}? {SAVE_KEYS}", f.display())
    );
    assert!(!h.quit());
    h.keys("y");
    assert!(h.quit());
    assert_eq!(read(&f), "xone\n");
}

#[test]
fn kill_terminal_asks_about_unsaved_buffers_in_full_words() {
    let f = file("one\n");
    let mut h = emacs(&f);
    h.keys("x<C-x><C-c>n");
    assert_eq!(
        h.status(),
        "Modified buffers exist; exit anyway? (yes or no) "
    );
    h.keys("ma<CR>");
    assert_eq!(h.status(), "Please answer yes or no.");
    assert!(!h.quit());
    h.keys("no<CR>");
    assert!(!h.quit(), "no keeps the session");
    h.keys("<C-x><C-c>n");
    h.keys("yes<CR>");
    assert!(h.quit());
    assert_eq!(read(&f), "one\n", "nothing was saved");
    // `C-g` quits the question.
    let mut h = emacs(&f);
    h.keys("x<C-x><C-c><C-g>");
    assert_eq!(h.status(), "Quit");
    assert!(!h.quit());
    // A clean session exits at once.
    let mut h = emacs(&f);
    h.keys("<C-x><C-c>");
    assert!(h.quit());
}

#[test]
fn save_some_buffers_answers() {
    let f = file("one\n");
    let mut h = emacs(&f);
    h.keys("<C-x>s");
    assert_eq!(h.status(), "(No files need saving)");
    h.keys("x<C-x>s");
    assert_eq!(
        h.status(),
        format!("Save file {}? {SAVE_KEYS}", f.display())
    );
    // A key that is no answer keeps the question.
    h.keys("z");
    assert!(h.status().starts_with("Save file "), "{}", h.status());
    h.keys("n");
    assert_eq!(read(&f), "one\n");
    // `M-~` skips and marks the buffer unmodified, so C-x C-c then exits without asking.
    h.keys("<C-x>s<M-~><C-x><C-c>");
    assert!(h.quit());
    let mut h = emacs(&f);
    h.keys("x<C-x>s!");
    assert_eq!(read(&f), "xone\n");
}

#[test]
fn revert_buffer_and_revert_buffer_quick() {
    let f = file("one\n");
    let shown = f.display().to_string();
    let mut h = emacs(&f);
    h.keys("x<M-x>revert-buffer<CR>");
    assert_eq!(
        h.status(),
        format!("Discard edits and reread from {shown}? (yes or no) ")
    );
    h.keys("yes<CR>");
    assert_eq!(h.text(), "one\n");
    h.keys("<M-x>revert-buffer<CR>");
    assert_eq!(
        h.status(),
        format!("Revert buffer from file {shown}? (yes or no) ")
    );
    h.keys("<C-g>");
    // `C-x x g` reverts an unmodified buffer without asking; the revert is undoable.
    std::fs::write(&f, "two\n").unwrap();
    h.keys("<C-x>xg");
    assert_eq!(h.status(), "Reverted buffer");
    assert_eq!(h.text(), "two\n");
    h.keys("x<C-x>xg");
    assert_eq!(
        h.status(),
        format!("Discard edits and reread from {shown}? (yes or no) ")
    );
    // Without a file there is nothing to revert.
    let mut h = Headless::emacs("a\n");
    h.keys("<M-x>revert-buffer<CR>");
    assert_eq!(
        h.status(),
        "Buffer does not seem to be associated with any file"
    );
}

#[test]
fn read_only_mode_refuses_changes() {
    let f = file("one\n");
    let mut h = emacs(&f);
    h.keys("<C-x><C-q>");
    assert_eq!(h.status(), "Read-Only mode enabled in current buffer");
    h.keys("a");
    assert_eq!(h.status(), "Buffer is read-only: #<buffer f.txt>");
    assert_eq!(h.text(), "one\n");
    h.keys("<C-x><C-q>");
    assert_eq!(h.status(), "Read-Only mode disabled in current buffer");
    h.keys("a");
    assert_eq!(h.text(), "aone\n");
}

#[test]
fn find_file_of_a_changed_file_asks_to_reread() {
    let f = file("one\n");
    let mut h = emacs(&f);
    std::fs::write(&f, "outside\n").unwrap();
    h.keys("<C-x><C-f>");
    assert!(h.status().starts_with("Find file: "), "{}", h.status());
    // Clear the default directory and type the absolute name.
    let back = "<BS>".repeat(h.status().len() - "Find file: ".len());
    h.keys(&format!("{back}{}<CR>", f.display()));
    assert_eq!(
        h.status(),
        "File f.txt changed on disk.  Reread from disk? (yes or no) "
    );
    h.keys("yes<CR>");
    assert_eq!(h.text(), "outside\n");
}
