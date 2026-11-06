//! What `:messages` keeps (#1235), driven through the headless session. The expected histories are nvim
//! v0.12.6's, probed with `--clean` in a PTY (`execute('messages')` after each command): errors, warnings
//! and reports are kept; option replies, file messages, `ga` and command output are not, nor anything a
//! `:silent` hides (a `:silent` error is kept, a `:silent!` one is not).

use ruse_tui::headless::Headless;

fn messages(keys: &str) -> Vec<String> {
    let mut h = Headless::vim("a\nb\nc\nfoo\nfoo\nx\n");
    h.keys(keys);
    h.messages()
}

#[test]
fn errors_and_reports_are_kept() {
    assert_eq!(
        messages(":nosuch<CR>/zzz<CR>:1,3y<CR>"),
        [
            "E492: Not an editor command: nosuch",
            "E486: Pattern not found: zzz",
            "3 lines yanked",
        ]
    );
}

#[test]
fn replies_and_output_are_not_kept() {
    assert!(
        messages(":set ts?<CR>ga:ls<CR>:1,2p<CR>:messages<CR>").is_empty(),
        "nothing kept"
    );
}

#[test]
fn silent_keeps_its_error_and_silent_bang_keeps_nothing() {
    assert_eq!(
        messages(":silent 1,3d<CR>:silent nosuch<CR>:silent! nosuch2<CR>"),
        ["E492: Not an editor command: nosuch"]
    );
}

#[test]
fn a_global_keeps_one_report() {
    // nvim's `global_busy` holds back each command's report; the `:g` gives one, for the lines all its
    // commands added (#1276): two copies are within 'report', three are not.
    assert!(messages(":g/foo/t$<CR>").is_empty());
    assert_eq!(messages(":g/foo\\|a/t$<CR>"), ["3 more lines"]);
}

#[test]
fn messages_shows_and_clears_the_history() {
    let mut h = Headless::vim("a\nb\nc\n");
    h.keys(":x1<CR>:x2<CR>:x3<CR>:2messages<CR>");
    assert_eq!(
        h.status(),
        "E492: Not an editor command: x2\nE492: Not an editor command: x3"
    );
    h.keys(":messages<CR>");
    assert_eq!(h.status().lines().count(), 3);
    h.keys(":1messages clear<CR>");
    assert_eq!(h.messages(), ["E492: Not an editor command: x3"]);
    h.keys(":messages clear<CR>:messages<CR>");
    assert!(h.messages().is_empty());
    assert_eq!(h.status(), "");
    h.keys(":messages foo<CR>");
    assert_eq!(h.status(), "E474: Invalid argument");
}

#[test]
fn messagesopt_sizes_the_history() {
    let mut h = Headless::vim("a\n");
    h.keys(":x1<CR>:x2<CR>:x3<CR>:set mopt=hit-enter,history:2<CR>");
    assert_eq!(
        h.messages(),
        [
            "E492: Not an editor command: x2",
            "E492: Not an editor command: x3"
        ]
    );
    h.keys(":set mopt?<CR>");
    assert_eq!(h.status(), "  messagesopt=hit-enter,history:2");
    h.keys(":set mopt=history:3<CR>");
    assert_eq!(h.status(), "E474: Invalid argument: mopt=history:3");
    h.keys(":set mopt=hit-enter,history:0<CR>:x4<CR>");
    assert!(h.messages().is_empty(), "history:0 keeps nothing");
}
