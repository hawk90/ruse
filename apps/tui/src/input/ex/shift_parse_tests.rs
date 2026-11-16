use super::*;

#[test]
fn shift_parses_range_direction_and_levels() {
    // No range → current line, one level.
    assert_eq!(
        parse_ex(">"),
        Ex::Shift {
            range: SubRange::CurrentLine,
            left: false,
            levels: 1
        }
    );
    assert_eq!(
        parse_ex("<"),
        Ex::Shift {
            range: SubRange::CurrentLine,
            left: true,
            levels: 1
        }
    );
    // Repeated verb chars are the level count (Vim `:>>>` = 3, `:<<` = 2).
    assert_eq!(
        parse_ex(">>>"),
        Ex::Shift {
            range: SubRange::CurrentLine,
            left: false,
            levels: 3
        }
    );
    assert_eq!(
        parse_ex("<<"),
        Ex::Shift {
            range: SubRange::CurrentLine,
            left: true,
            levels: 2
        }
    );
    // A line range and the whole-file `%`.
    assert_eq!(
        parse_ex("2,4>"),
        Ex::Shift {
            range: SubRange::Lines(2, 4),
            left: false,
            levels: 1
        }
    );
    assert_eq!(
        parse_ex("%<<"),
        Ex::Shift {
            range: SubRange::WholeFile,
            left: true,
            levels: 2
        }
    );
}

#[test]
fn mixed_run_or_trailing_junk_is_not_a_shift() {
    // A mixed `>`/`<` run is not this command.
    assert!(matches!(parse_ex("><"), Ex::Unknown(_)));
    // A count (`:> 5`) is folded into the range before parsing; with trailing junk it is not a shift.
    assert!(matches!(parse_ex("> 5"), Ex::Unknown(_)));
    assert!(matches!(parse_ex(">x"), Ex::Unknown(_)));
}
