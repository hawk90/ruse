use super::*;

#[test]
fn read_file_forms_parse_addr_and_path() {
    // Bare `:r`/`:read` → below the current line.
    assert_eq!(
        parse_ex("r ins.txt"),
        Ex::Read {
            addr: LineAddr::Current,
            source: ReadSource::File("ins.txt".into())
        }
    );
    assert_eq!(
        parse_ex("read ins.txt"),
        Ex::Read {
            addr: LineAddr::Current,
            source: ReadSource::File("ins.txt".into())
        }
    );
    // A line address: `:2r` below line 2, `:0r` at the top, `:$r` after the last line.
    assert_eq!(
        parse_ex("2r ins.txt"),
        Ex::Read {
            addr: LineAddr::Line(2),
            source: ReadSource::File("ins.txt".into())
        }
    );
    assert_eq!(
        parse_ex("0r ins.txt"),
        Ex::Read {
            addr: LineAddr::Line(0),
            source: ReadSource::File("ins.txt".into())
        }
    );
    assert_eq!(
        parse_ex("$r ins.txt"),
        Ex::Read {
            addr: LineAddr::Last,
            source: ReadSource::File("ins.txt".into())
        }
    );
}

#[test]
fn read_command_form_parses_after_the_bang() {
    assert_eq!(
        parse_ex("r !sort"),
        Ex::Read {
            addr: LineAddr::Current,
            source: ReadSource::Command("sort".into())
        }
    );
    // Leading space after `!` is trimmed; the rest of the command is verbatim.
    assert_eq!(
        parse_ex("3r ! ls -la"),
        Ex::Read {
            addr: LineAddr::Line(3),
            source: ReadSource::Command("ls -la".into())
        }
    );
    // The `!` may follow the verb directly (#809).
    for line in ["r!echo hi", "read!echo hi", "r! echo hi"] {
        assert_eq!(
            parse_ex(line),
            Ex::Read {
                addr: LineAddr::Current,
                source: ReadSource::Command("echo hi".into())
            },
            "{line}"
        );
    }
    assert_eq!(
        parse_ex("0r!echo hi"),
        Ex::Read {
            addr: LineAddr::Line(0),
            source: ReadSource::Command("echo hi".into())
        }
    );
}

/// `:d`/`:y` take a register after the verb, and every abbreviation of the verb (#608).
#[test]
fn delete_and_yank_take_a_register() {
    assert_eq!(
        parse_ex("d a"),
        Ex::Delete(SubRange::CurrentLine, Some('a'), None)
    );
    assert_eq!(
        parse_ex("2,3del _"),
        Ex::Delete(SubRange::Lines(2, 3), Some('_'), None)
    );
    assert_eq!(
        parse_ex("de"),
        Ex::Delete(SubRange::CurrentLine, None, None)
    );
    assert_eq!(
        parse_ex("yan A"),
        Ex::Yank(SubRange::CurrentLine, Some('A'))
    );
    assert!(matches!(parse_ex("d ab"), Ex::Unknown(_)));
    assert!(matches!(parse_ex("dx"), Ex::Unknown(_)));
}

/// #1241: `:delete` spelled with a trailing `l`/`p` is `:delete` with that print flag, as Vim's
/// `find_ex_command` reads it (nvim v0.12.6).
#[test]
fn dl_and_dp_are_delete_with_a_print_flag() {
    let list = Some(PrintFlags {
        number: false,
        list: true,
    });
    let print = Some(PrintFlags::default());
    for (verb, flags) in [
        ("dl", list),
        ("dell", list),
        ("delel", list),
        ("deletl", list),
        ("deletel", list),
        ("dp", print),
        ("dep", print),
        ("delp", print),
        ("deletp", print),
        ("deletep", print),
        ("del", None),
        ("dele", None),
        ("dll", None),
        ("dlp", None),
        ("dpl", None),
        ("dela", None),
        ("deletell", None),
        ("l", None),
        ("", None),
    ] {
        assert_eq!(delete_print_flags(verb), flags, "{verb:?}");
    }
    assert_eq!(
        parse_ex("3dl a"),
        Ex::Delete(SubRange::Lines(3, 3), Some('a'), list)
    );
    assert_eq!(
        parse_ex("deletep"),
        Ex::Delete(SubRange::CurrentLine, None, print)
    );
    assert!(matches!(parse_ex("dll"), Ex::Unknown(_)));
}

/// #1241: `:mark`/`:k` take every mark nvim's `setmark_pos` sets; anything else is E191, a bang E477,
/// and a `"` starts a comment.
#[test]
fn mark_takes_the_marks_setmark_sets() {
    for name in ['a', 'Z', '0', '9', '\'', '`', '[', ']', '<', '>'] {
        assert_eq!(
            parse_ex(&format!("3k{name}")),
            Ex::Mark {
                range: SubRange::Lines(3, 3),
                name
            },
            "{name:?}"
        );
        assert_eq!(
            parse_ex(&format!("mark {name}")),
            Ex::Mark {
                range: SubRange::CurrentLine,
                name
            },
            "{name:?}"
        );
    }
    let fail = |s: &str| Ex::Fail(s.to_string());
    let e191 = fail("E191: Argument must be a letter or forward/backward quote");
    for bad in ["k^", "k.", "k(", "k#", "k-", "k{", "mark ^"] {
        assert_eq!(parse_ex(bad), e191, "{bad:?}");
    }
    assert_eq!(parse_ex("k!"), fail("E477: No ! allowed"));
    assert_eq!(parse_ex("mark! a"), fail("E477: No ! allowed"));
    assert_eq!(parse_ex("k\""), fail(ARGUMENT_REQUIRED));
    assert_eq!(
        parse_ex("3k a \" why"),
        Ex::Mark {
            range: SubRange::Lines(3, 3),
            name: 'a'
        }
    );
}

#[test]
fn bang_run_vs_filter_is_decided_by_the_range() {
    // No range → `:!cmd` runs and shows output.
    assert_eq!(parse_ex("!ls"), Ex::Shell("ls".into()));
    assert_eq!(parse_ex("!echo hi"), Ex::Shell("echo hi".into()));
    // A range → `:{range}!cmd` filters.
    assert_eq!(
        parse_ex("%!sort"),
        Ex::Filter {
            range: SubRange::WholeFile,
            cmd: "sort".into()
        }
    );
    assert_eq!(
        parse_ex("2,3!sort"),
        Ex::Filter {
            range: SubRange::Lines(2, 3),
            cmd: "sort".into()
        }
    );
    // An explicit `.` is a range (current line), so it filters — distinct from a bare `:!`.
    assert_eq!(
        parse_ex(".!tr a-z A-Z"),
        Ex::Filter {
            range: SubRange::CurrentLine,
            cmd: "tr a-z A-Z".into()
        }
    );
}

#[test]
fn non_read_and_empty_forms_fall_through() {
    // A bare `:r`/`:read` (no argument — re-read the current file) is a documented non-goal.
    assert!(matches!(parse_ex("r"), Ex::Unknown(_)));
    assert!(matches!(parse_ex("read"), Ex::Unknown(_)));
    // `:r !` with no command is not a valid read.
    assert!(matches!(parse_ex("r !"), Ex::Unknown(_)));
    // A `!` with no command, and a range-only `!`, are not shell forms.
    assert!(matches!(parse_ex("!"), Ex::Unknown(_)));
    assert!(matches!(parse_ex("%!"), Ex::Unknown(_)));
    // `:registers`/`:rename`/`:redo` are NOT swallowed by the `:r` verb.
    assert_eq!(parse_ex("registers"), Ex::Registers);
    assert_eq!(parse_ex("rename foo"), Ex::Rename("foo".into()));
    assert!(matches!(parse_ex("redo"), Ex::Unknown(_)));
}
