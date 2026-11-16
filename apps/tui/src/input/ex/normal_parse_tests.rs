use super::*;

fn normal(line: &str) -> (bool, Option<SubRange>, String) {
    match parse_ex(line) {
        Ex::Normal { bang, range, keys } => (bang, range, keys),
        other => panic!("expected Ex::Normal, got {other:?}"),
    }
}

#[test]
fn no_range_takes_the_whole_verbatim_payload() {
    assert_eq!(normal("normal dwx"), (false, None, "dwx".to_string()));
    // The abbreviations `norm`/`norma` resolve the same.
    assert_eq!(normal("norm dwx"), (false, None, "dwx".to_string()));
    assert_eq!(normal("norma dwx"), (false, None, "dwx".to_string()));
    // Only the FIRST space is the delimiter; further spaces are part of the keys.
    assert_eq!(normal("normal f x"), (false, None, "f x".to_string()));
}

#[test]
fn bang_is_parsed_and_accepted() {
    assert_eq!(normal("normal! A;"), (true, None, "A;".to_string()));
}

#[test]
fn range_prefixes_resolve() {
    assert_eq!(
        normal("%normal A;"),
        (false, Some(SubRange::WholeFile), "A;".to_string())
    );
    assert_eq!(
        normal("2,3normal 0x"),
        (false, Some(SubRange::Lines(2, 3)), "0x".to_string())
    );
    assert_eq!(
        normal(".normal x"),
        (false, Some(SubRange::CurrentLine), "x".to_string())
    );
}

#[test]
fn join_parses_range_verb_and_bang() {
    // No range → current line.
    assert_eq!(
        parse_ex("j"),
        Ex::Join {
            range: SubRange::CurrentLine,
            bang: false
        }
    );
    // The abbreviations `jo`/`joi`/`join` all resolve.
    for v in ["jo", "joi", "join"] {
        assert_eq!(
            parse_ex(v),
            Ex::Join {
                range: SubRange::CurrentLine,
                bang: false
            }
        );
    }
    // A line range.
    assert_eq!(
        parse_ex("2,4join"),
        Ex::Join {
            range: SubRange::Lines(2, 4),
            bang: false
        }
    );
    // The `!` (raw `gJ`) form.
    assert_eq!(
        parse_ex("j!"),
        Ex::Join {
            range: SubRange::CurrentLine,
            bang: true
        }
    );
    assert_eq!(
        parse_ex("%join!"),
        Ex::Join {
            range: SubRange::WholeFile,
            bang: true
        }
    );
    // `:jumps` is NOT a join (its exact-match arm wins first).
    assert_eq!(parse_ex("jumps"), Ex::Jumps);
}

#[test]
fn key_notation_stays_verbatim_in_the_payload() {
    // The payload is stored VERBATIM (and run literally, #815).
    assert_eq!(
        normal("normal Ihi<Esc>"),
        (false, None, "Ihi<Esc>".to_string())
    );
}

#[test]
fn non_normal_and_bare_normal_fall_through() {
    // `:normalx` is not the `:normal` verb.
    assert!(matches!(parse_ex("normalx"), Ex::Unknown(_)));
    // A bare `:normal` (no keys) is E471, as in nvim.
    let e471 = Ex::Fail("E471: Argument required".into());
    assert_eq!(parse_ex("normal"), e471);
    assert_eq!(parse_ex("normal!"), e471);
    assert_eq!(parse_ex("norm "), e471);
}
