//! What the Ex print commands and the other #609/#610 commands show in the message area, checked against
//! nvim v0.12.6's output (`print_line`, `ex_z`, `:=`, `:pwd`, `:file`). The buffer and cursor effects are
//! pinned by the oracle corpus (`exl_*` fixtures); this covers the text the corpus does not capture.

use ruse_tui::headless::Headless;

fn status_after(text: &str, keys: &str) -> String {
    let mut h = Headless::vim(text);
    h.keys(keys);
    h.status().to_string()
}

#[test]
fn print_commands_show_lines_like_nvim() {
    // One line goes on the status line; more open the listing, with the last on the status line.
    assert_eq!(status_after("a\nb\nc\nd", ":2p<CR>"), "b");
    assert_eq!(status_after("a\nb\nc\nd", ":2,3p<CR>"), "c");
    assert_eq!(status_after("a\nb\nc\nd", ":2#<CR>"), "  2 b");
    assert_eq!(status_after("a\nb\nc\nd", ":3nu<CR>"), "  3 c");
    // `:list` with ruse's default tabstop of 4: a tab is `>` and blanks, trailing spaces `-`.
    assert_eq!(status_after("a\tb  \nx", ":l<CR>"), "a>  b--");
    assert_eq!(status_after("a\u{1}b", ":p<CR>"), "a^Ab");
    assert_eq!(status_after("aa\naa", ":s/a/b/#<CR>"), "  1 ba");
    assert_eq!(status_after("ab\nb\ncb", ":g/b/nu<CR>"), "  3 cb");
}

#[test]
fn line_number_and_filter() {
    assert_eq!(status_after("a\nb\nc\nd", ":=<CR>"), "4");
    assert_eq!(status_after("a\nb\nc\nd", ":.=<CR>"), "1");
    assert_eq!(status_after("ab\nb\ncb", ":filter /c/ %p<CR>"), "cb");
    assert_eq!(status_after("ab\nb\ncb", ":filter! /c/ %p<CR>"), "b");
    assert_eq!(status_after("ab\nb\ncb", ":filt c 1,2p<CR>"), "");
    assert_eq!(
        status_after("ab", ":filter /c/<CR>"),
        "E476: Invalid command"
    );
}

#[test]
fn file_and_directory_messages() {
    let cwd = std::env::current_dir().unwrap();
    assert_eq!(status_after("a", ":pwd<CR>"), cwd.display().to_string());
    assert_eq!(
        status_after("a\nb", ":f<CR>"),
        "\"[No Name]\" 2 lines --50%--"
    );
    assert_eq!(
        status_after("a\nb", ":file x y<CR>:f<CR>"),
        "\"x y\" 2 lines --50%--",
        "the whole argument is the name"
    );
    assert_eq!(
        status_after("a\nb", ":file x<CR>:0f<CR>:f<CR>"),
        "\"[No Name]\" 2 lines --50%--"
    );
    assert_eq!(
        status_after("a", ":find nosuch<CR>"),
        "E345: Can't find file \"nosuch\" in path"
    );
    assert_eq!(status_after("a", ":find<CR>"), "E471: Argument required");
    assert_eq!(status_after("a", ":drop<CR>"), "E471: Argument required");
    assert_eq!(
        status_after("a", ":cd -<CR>"),
        "E186: No previous directory"
    );
}
