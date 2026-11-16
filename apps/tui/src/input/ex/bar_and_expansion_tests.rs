//! #601: `|` chaining, `:sig`, and the `!` / `%` / `#` expansions. Behaviour probed against nvim v0.12.6.

use super::*;

fn split(line: &str) -> Vec<String> {
    split_bar(line)
}

#[test]
fn bar_separates_commands() {
    assert_eq!(split("s/a/b/|d"), ["s/a/b/", "d"]);
    assert_eq!(split("s/a/b/g|d"), ["s/a/b/g", "d"]);
    assert_eq!(split("s/a/b/ | d"), ["s/a/b/ ", "d"]);
    assert_eq!(
        split("set ts=8|set sw=4|:set noet"),
        ["set ts=8", "set sw=4", "set noet"]
    );
    assert_eq!(split("%s/a/b/|d"), ["%s/a/b/", "d"]);
    assert_eq!(split("w"), ["w"]);
}

#[test]
fn substitute_keeps_a_bar_in_its_pattern_and_replacement() {
    assert_eq!(split("s/a|b/X/"), ["s/a|b/X/"]);
    assert_eq!(split("s/a/X|Y/"), ["s/a/X|Y/"]);
    assert_eq!(split("s/a/X|Y/|d"), ["s/a/X|Y/", "d"]);
    // An unterminated replacement runs to the end of the line, `|` included.
    assert_eq!(split("s/a/X|Y"), ["s/a/X|Y"]);
    // A regex alternation `\|` is never a separator, and keeps its backslash.
    assert_eq!(split(r"s/a\|b/X/|d"), [r"s/a\|b/X/", "d"]);
    assert_eq!(split(r"sort /a\|b/"), [r"sort /a\|b/"]);
}

#[test]
fn global_normal_and_shell_own_the_rest_of_the_line() {
    assert_eq!(split("g/a/d|d"), ["g/a/d|d"]);
    assert_eq!(split("v/a/s/x/y/|d"), ["v/a/s/x/y/|d"]);
    assert_eq!(split("normal ix|y"), ["normal ix|y"]);
    assert_eq!(split("!ls | wc -l"), ["!ls | wc -l"]);
    assert_eq!(split("%!sort | uniq"), ["%!sort | uniq"]);
    assert_eq!(split("r !ls | wc"), ["r !ls | wc"]);
    // `:r {file}` is an ordinary command.
    assert_eq!(split("r f.txt|d"), ["r f.txt", "d"]);
}

#[test]
fn sig_is_not_a_repeat_substitute() {
    // nvim reads `:sig` as `:sign` (E471), `:sil`/`:sim` as `:silent`/`:simalt`.
    assert_eq!(parse_ex("sig"), Ex::Fail("E471: Argument required".into()));
    // The other `:s` + flag shorthands still repeat.
    for line in ["si", "sg", "sgi", "sI", "s ig"] {
        assert!(
            matches!(parse_ex(line), Ex::RepeatSubstitute { .. }),
            "{line:?}"
        );
    }
}

#[test]
fn bang_expands_to_the_previous_command() {
    assert_eq!(expand_bang("!", Some("ls -l")), Ok("ls -l".into()));
    assert_eq!(expand_bang("! | wc", Some("ls")), Ok("ls | wc".into()));
    assert_eq!(expand_bang(r"echo a\!b", None), Ok("echo a!b".into()));
    assert_eq!(expand_bang("echo hi", None), Ok("echo hi".into()));
    assert_eq!(expand_bang("!", None), Err(ExpandError::NoPreviousCommand));
    // `:!!` parses as a shell run of `!`, which the session expands.
    assert_eq!(parse_ex("!!"), Ex::Shell("!".into()));
}

#[test]
fn percent_and_hash_expand_to_the_file_names() {
    let buffers = [(1, "src/a.rs".to_string()), (3, "dir/k.tar.gz".to_string())];
    let names = FileNames {
        current: Some("src/a.rs"),
        alternate: Some("b.txt"),
        buffers: &buffers,
    };
    let x = |arg: &str| expand_filename(arg, &names);
    assert_eq!(x("%"), Ok("src/a.rs".into()));
    assert_eq!(x("#"), Ok("b.txt".into()));
    assert_eq!(x("cat % #"), Ok("cat src/a.rs b.txt".into()));
    assert_eq!(x(r"a\%b\#"), Ok("a%b#".into()));
    // `#{n}` is buffer n's file; a number with no such buffer is E194 (nvim v0.12.6).
    assert_eq!(x("#3"), Ok("dir/k.tar.gz".into()));
    assert_eq!(x("#2"), Err(ExpandError::NoAlternateFile));
    let no_cur = FileNames {
        current: None,
        ..names
    };
    assert_eq!(
        expand_filename("%", &no_cur),
        Err(ExpandError::EmptyFileName)
    );
    let no_alt = FileNames {
        alternate: None,
        ..names
    };
    assert_eq!(
        expand_filename("#", &no_alt),
        Err(ExpandError::NoAlternateFile)
    );
    assert!(ExpandError::EmptyFileName.to_string().starts_with("E499:"));
    assert!(ExpandError::NoAlternateFile
        .to_string()
        .starts_with("E194:"));
    assert_eq!(
        ExpandError::NoPreviousCommand.to_string(),
        "E34: No previous command"
    );
}
