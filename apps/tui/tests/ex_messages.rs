//! What the Ex print commands and the other #609/#610 commands show in the message area, checked against
//! nvim v0.12.6's output (`print_line`, `ex_z`, `:=`, `:pwd`, `:file`). The buffer and cursor effects are
//! pinned by the oracle corpus (`exl_*` fixtures); this covers the text the corpus does not capture, and the
//! error a failed `"=` expression shows (#1570).

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

/// `:put ={expr}` (#1342), against nvim v0.12.6: the result goes into `"=` and is put as lines; a bad
/// expression is E15 naming it, and puts nothing; an empty one is the last expression, whether typed in a
/// `:put =`, at a `"=` prompt or at an Insert `<C-r>=` prompt.
#[test]
fn put_expression_puts_its_result_and_reports_a_bad_one() {
    let mut h = Headless::vim("a\n");
    h.keys(":put ='x'..'y'<CR>");
    assert_eq!(h.text(), "a\nxy\n");
    assert_eq!(h.registers().get(Some('=')).text(), b"xy");
    assert_eq!(h.status(), "");
    // An empty expression reuses the last one; `''` still puts one (empty) line.
    h.keys(":0put =<CR>:$put =''<CR>");
    assert_eq!(h.text(), "xy\na\nxy\n\n");

    let mut h = Headless::vim("a\nb\nc\nd\n");
    h.keys(":3put =1+<CR>");
    assert_eq!(h.text(), "a\nb\nc\nd\n");
    assert_eq!(h.status(), "E15: Invalid expression: \"1+\"");
    // The cursor is on the addressed line, as nvim's `ex_put` puts it before it reads the register.
    assert_eq!(h.cursor_row_col(), (3, 0));
    // The bad expression is the last one now.
    h.keys(":put =<CR>");
    assert_eq!(h.status(), "E15: Invalid expression: \"1+\"");

    // With no expression ever typed, an empty one puts nothing and says nothing.
    assert_eq!(status_after("a\n", ":put =<CR>"), "");

    let mut h = Headless::vim("a\n");
    h.keys("\"='q'<CR>p:put =<CR>");
    assert_eq!(h.text(), "aq\nq\n");
    let mut h = Headless::vim("a\n");
    h.keys("i<C-r>='w'<CR><Esc>:put =<CR>");
    assert_eq!(h.text(), "wa\nw\n");
}

/// #1570: a failed `"=` expression shows nvim v0.12.6's error at every entry point — `"=…p`, `"=…P`,
/// Insert `<C-r>=` and `:put =` — and puts nothing; Insert mode carries on after it, as in nvim.
#[test]
fn failed_expression_reports_nvims_error_at_every_entry_point() {
    let cases = [
        ("1+", r#"E15: Invalid expression: "1+""#),
        ("0x", r#"E15: Invalid expression: "0x""#),
        ("nosuchvar", "E121: Undefined variable: nosuchvar"),
        ("1'000", "E488: Trailing characters: '000"),
    ];
    for (expr, want) in cases {
        let code = want.split(':').next().expect("an E-number");
        for keys in [
            format!("\"={expr}<CR>p"),
            format!("\"={expr}<CR>P"),
            format!(":put ={expr}<CR>"),
        ] {
            let mut h = Headless::vim("a\n");
            h.keys(&keys);
            assert_eq!(h.text(), "a\n", "{keys}");
            assert_eq!(h.status(), want, "{keys}");
            assert_eq!(h.emsg(), code, "{keys}");
        }
        let keys = format!("i<C-r>={expr}<CR>X<Esc>");
        let mut h = Headless::vim("a\n");
        h.keys(&keys);
        assert_eq!(h.text(), "Xa\n", "{keys}");
        assert_eq!(h.emsg(), code, "{keys}");
    }
    // A good expression says nothing.
    assert_eq!(status_after("a\n", "\"=1+2<CR>p"), "");
    let mut h = Headless::vim("a\n");
    h.keys("i<C-r>=1+2<CR><Esc>");
    assert_eq!(h.text(), "3a\n");
    assert_eq!(h.emsg(), "");
}
