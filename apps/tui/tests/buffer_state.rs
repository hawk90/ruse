//! Buffer state and leaving changed buffers (#619), against nvim v0.12.6 (`nvim --clean --headless`, the
//! errors read from `v:errmsg`): `'modifiable'` (E21 for every change and undo, `:w` still writes),
//! `'modified'`, `'hidden'` off (E37 when a command would leave a changed buffer no other window shows,
//! E445 for `:only`), and the `'confirm'` / `:confirm` dialog.

use ruse_tui::headless::Headless;

const E21: &str = "E21: Cannot make changes, 'modifiable' is off";
const E37: &str = "E37: No write since last change (add ! to override)";

fn vim(keys: &str) -> Headless {
    let mut h = Headless::vim("one\n");
    h.keys(keys);
    h
}

#[test]
fn nomodifiable_refuses_every_change() {
    for keys in [
        ":set noma<CR>x",
        ":set noma<CR>dd",
        ":set noma<CR>:s/o/X/<CR>",
        ":set noma<CR>:d<CR>",
        ":set noma<CR>p",
        ":set noma<CR>yyp",
    ] {
        let h = vim(keys);
        assert_eq!(h.status(), E21, "{keys}");
        assert_eq!(h.text(), "one\n", "{keys}");
    }
    // Undo steps the text too.
    let h = vim("x:set noma<CR>u");
    assert_eq!(h.status(), E21);
    assert_eq!(h.text(), "ne\n");
    // Moving and yanking are no changes; `:set ma` lets changes in again.
    let h = vim(":set noma<CR>yyj:set ma<CR>x");
    assert_eq!(h.text(), "ne\n");
    assert_eq!(vim(":set noma<CR>:set ma?<CR>").status(), "nomodifiable");
    assert_eq!(vim(":set ma?<CR>").status(), "  modifiable");
}

#[test]
fn modified_reads_and_sets_the_buffer_state() {
    assert_eq!(vim(":set mod?<CR>").status(), "nomodified");
    assert_eq!(vim("x:set mod?<CR>").status(), "  modified");
    // `:set nomod` marks it unchanged, so `:q` quits; `:set mod` the other way.
    let h = vim("x:set nomod<CR>:q<CR>");
    assert!(h.quit());
    assert_eq!(h.text(), "ne\n");
    let h = vim(":set mod<CR>:q<CR>");
    assert!(!h.quit());
    assert!(h.status().starts_with("E37"), "{}", h.status());
}

#[test]
fn nohidden_refuses_to_leave_a_changed_buffer() {
    assert_eq!(vim(":set hid?<CR>").status(), "  hidden");
    let h = vim(":set nohid<CR>x:enew<CR>");
    assert_eq!(h.status(), E37);
    // With 'hidden' (the default) the buffer stays loaded and hidden.
    let h = vim("x:enew<CR>");
    assert!(h.status().contains("[No Name]"), "{}", h.status());
    // Another window still shows it: no check.
    let h = vim(":set nohid<CR>x:split<CR>:enew<CR>");
    assert!(h.status().contains("[No Name]"), "{}", h.status());
    // `:b` back to a changed buffer's twin and `:bn`.
    let h = vim(":enew<CR>:set nohid<CR>ix<Esc>:bn<CR>");
    assert_eq!(h.status(), E37);
    // A window over a changed buffer no other window shows does not close.
    let h = vim(":split<CR>:enew<CR>ix<Esc>:set nohid<CR>:q<CR>");
    assert_eq!(h.status(), E37);
    let h = vim(":split<CR>:enew<CR>ix<Esc>:set nohid<CR>:close<CR>");
    assert_eq!(h.status(), E37);
    // `:only` over another window's changes is E445.
    let h = vim(":split<CR>:enew<CR>ix<Esc><C-w>w:set nohid<CR>:only<CR>");
    assert_eq!(h.status(), "E445: Other window contains changes");
}

#[test]
fn confirm_asks_and_the_answer_decides() {
    let prompt = "Save changes to \"Untitled\"? [Y]es, (N)o, (C)ancel: ";
    let mut h = vim("x:confirm q<CR>");
    assert_eq!(h.status(), prompt);
    assert!(!h.quit());
    // Keys that are no answer leave the prompt up; `n` marks the buffer unchanged and quits.
    h.keys("z");
    assert!(!h.quit());
    h.keys("n");
    assert!(h.quit());
    // `<Esc>` cancels.
    let mut h = vim("x:confirm q<CR>");
    h.keys("<Esc>");
    assert!(!h.quit());
    assert!(h.status().is_empty(), "{}", h.status());
    // 'confirm' asks for every such command, here `:enew` with 'nohidden'.
    let h = vim(":set cf nohid<CR>x:enew<CR>");
    assert_eq!(h.status(), prompt);
    // `y` on a buffer without a file cannot write it.
    let mut h = vim("x:confirm q<CR>");
    h.keys("y");
    assert!(!h.quit());
    assert!(h.status().starts_with("E141"), "{}", h.status());
}

#[test]
fn confirm_qa_offers_save_all_and_discard_all() {
    let mut h = vim("x:enew<CR>iy<Esc>:confirm qa<CR>");
    assert!(
        h.status()
            .ends_with("[Y]es, (N)o, Save (A)ll, (D)iscard All, (C)ancel: "),
        "{}",
        h.status()
    );
    h.keys("d");
    assert!(h.quit());
}

/// A trace saved after `:set noma` and an E21 refusal replays to the same text (#619): the buffer state
/// is in the trace (`ex_modifiable`, `ex_modified`), so the replay refuses the same edits.
#[test]
fn modifiable_and_modified_replay_from_the_trace() {
    for keys in [
        "x:set noma<CR>x:s/n/N/<CR>dd:set ma<CR>x",
        "x:set noma<CR>u:set ma<CR>u",
        "x:set nomod<CR>x",
    ] {
        let mut h = Headless::vim("one\ntwo\n");
        h.keys(keys);
        let path = std::env::temp_dir().join(format!(
            "ruse-bufstate-{}-{}.trace",
            std::process::id(),
            keys.len()
        ));
        h.keys(&format!("<Esc>:trace save {}<CR>", path.display()));
        assert!(
            h.status().starts_with("trace saved"),
            "{keys}: {}",
            h.status()
        );
        let saved = std::fs::read_to_string(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        assert!(
            saved.contains("ex_modifiable") || saved.contains("ex_modified"),
            "{saved}"
        );
        let st = ruse_core::Trace::from_text(&saved)
            .unwrap()
            .replay(b"one\ntwo\n")
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(st.bytes()),
            h.text(),
            "{keys}\n{saved}"
        );
    }
}
