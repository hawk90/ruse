use super::*;

fn drop_flags() -> Option<ruse_core::SubFlags> {
    Some(ruse_core::SubFlags::default())
}
fn g_flags() -> Option<ruse_core::SubFlags> {
    Some(ruse_core::SubFlags {
        global: true,
        ignore_case: None,
    })
}

#[test]
fn bare_s_and_ampersand_drop_flags_on_the_current_line() {
    // Bare `:s` and `:&` both repeat the last `:s` on the current line WITHOUT its flags.
    for line in ["s", "&"] {
        assert_eq!(
            parse_ex(line),
            Ex::RepeatSubstitute {
                range: SubRange::CurrentLine,
                flags: drop_flags(),
            },
            "`:{line}` = flag-less current-line repeat"
        );
    }
}

#[test]
fn double_ampersand_keeps_flags() {
    // `:&&` (a run of two or more `&`) KEEPS the stored flags (`flags = None`).
    assert_eq!(
        parse_ex("&&"),
        Ex::RepeatSubstitute {
            range: SubRange::CurrentLine,
            flags: None,
        }
    );
}

#[test]
fn s_with_only_flags_uses_the_given_flags() {
    // `:s g` and `:sg` (space optional) both mean "repeat with the given flag(s)".
    for line in ["s g", "sg"] {
        assert_eq!(
            parse_ex(line),
            Ex::RepeatSubstitute {
                range: SubRange::CurrentLine,
                flags: g_flags(),
            },
            "`:{line}` = repeat with `g`"
        );
    }
    // `i`/`I` case flags come through too.
    assert_eq!(
        parse_ex("s i"),
        Ex::RepeatSubstitute {
            range: SubRange::CurrentLine,
            flags: Some(ruse_core::SubFlags {
                global: false,
                ignore_case: Some(true),
            }),
        }
    );
}

#[test]
fn ranges_resolve_for_every_form() {
    assert_eq!(
        parse_ex("2,3&&"),
        Ex::RepeatSubstitute {
            range: SubRange::Lines(2, 3),
            flags: None,
        }
    );
    assert_eq!(
        parse_ex("%s"),
        Ex::RepeatSubstitute {
            range: SubRange::WholeFile,
            flags: drop_flags(),
        }
    );
    assert_eq!(
        parse_ex("5&"),
        Ex::RepeatSubstitute {
            range: SubRange::Lines(5, 5),
            flags: drop_flags(),
        }
    );
}

#[test]
fn real_substitute_and_other_verbs_are_not_repeat_forms() {
    // A real `:s/pat/rep/` stays a `Substitute`, not a repeat.
    assert!(matches!(parse_ex("s/a/b/"), Ex::Substitute(_)));
    // Other `s`-verbs and trailing junk fall through (they never reach the repeat parser or it rejects).
    assert!(matches!(parse_ex("sort"), Ex::Sort(..)));
    assert!(matches!(parse_ex("split"), Ex::Split));
    assert!(matches!(parse_ex("s 3"), Ex::Unknown(_))); // a count after a bare `:s` is deferred
    assert!(matches!(parse_ex("sx"), Ex::Unknown(_))); // `x` is not a repeat flag
}
