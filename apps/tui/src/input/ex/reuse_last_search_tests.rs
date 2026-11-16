use super::*;

fn sub(pattern: &str) -> Ex {
    Ex::Substitute(SubSpec {
        range: SubRange::CurrentLine,
        pattern: pattern.to_string(),
        replacement: "X".to_string(),
        global: false,
        ignore_case: None,
        confirm: false,
        count_only: false,
        quiet: false,
        keep_flags: false,
        count: None,
        print: None,
    })
}

fn pattern_of(ex: &Ex) -> &str {
    match ex {
        Ex::Substitute(s) => &s.pattern,
        _ => unreachable!(),
    }
}

#[test]
fn empty_pattern_borrows_the_last_search() {
    let mut ex = sub("");
    reuse_last_search(&mut ex, Some("foo")).unwrap();
    assert_eq!(pattern_of(&ex), "foo");
}

#[test]
fn non_empty_pattern_is_left_alone() {
    let mut ex = sub("bar");
    reuse_last_search(&mut ex, Some("foo")).unwrap();
    assert_eq!(
        pattern_of(&ex),
        "bar",
        "an explicit pattern wins over the last search"
    );
}

/// #1087: `:s//y/` before any search is `E35`, then `do_sub`'s `E476` (nvim v0.12.6).
#[test]
fn empty_pattern_without_a_last_search_is_e476() {
    let mut ex = sub("");
    assert_eq!(
        reuse_last_search(&mut ex, None).map_err(|e| e.to_string()),
        Err("E476: Invalid command: E35: No previous regular expression".into())
    );
}

/// #804: `:g//` takes the last search, and the `s//` inside `:g` takes the `:g` pattern.
#[test]
fn global_and_its_substitute_borrow_too() {
    let mut ex = parse_ex("g//s//X/");
    reuse_last_search(&mut ex, Some("a")).unwrap();
    let Ex::Global(spec) = &ex else {
        panic!("not a :g: {ex:?}")
    };
    assert_eq!(spec.pattern, "a");
    assert!(matches!(
        &spec.cmd,
        GlobalPayload::Core(GlobalCmd::Substitute { pattern, .. }) if pattern == "a"
    ));
    let mut ex = parse_ex("g/b/s//X/");
    reuse_last_search(&mut ex, Some("a")).unwrap();
    assert!(matches!(
        &ex,
        Ex::Global(GlobalSpec { cmd: GlobalPayload::Core(GlobalCmd::Substitute { pattern, .. }), .. })
            if pattern == "b"
    ));
    let mut ex = parse_ex("g//d");
    assert_eq!(
        reuse_last_search(&mut ex, None).map_err(|e| e.to_string()),
        Err("E476: Invalid command".to_string())
    );
}

#[test]
fn sort_borrows_or_reports_e35() {
    let mut ex = parse_ex("sort //");
    reuse_last_search(&mut ex, Some("x")).unwrap();
    assert!(matches!(&ex, Ex::Sort(_, o) if o.pattern.as_deref() == Some("x")));
    let mut ex = parse_ex("sort //");
    assert_eq!(
        reuse_last_search(&mut ex, None).map_err(|e| e.to_string()),
        Err("E35: No previous regular expression".to_string())
    );
}
