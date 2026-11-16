use super::k;
use crate::input::*;

// --- F-027 Lang-Arg translation stage (D-048 / RFC-0013) ---------------------------------------

fn ctrl_caret() -> KeyEvent {
    KeyEvent::new(KeyCode::Char('^'), KeyModifiers::CONTROL)
}

#[test]
fn lang_stage_translates_in_insert_one_substitution() {
    // Acceptance #1 + INV-FAIL-BOUNDED: a mapped key is rewritten exactly ONCE and dispatched
    // literally. A cyclic map (a<->b) cannot loop — `a` becomes `b`, never re-translated back to `a`.
    let mut e = InputEngine::new();
    e.lang_map.insert('a', 'b');
    e.lang_map.insert('b', 'a');
    e.lang_active = true;
    assert_eq!(
        e.feed(k('a'), Mode::Insert),
        Feed::Cmd(Command::InsertChar('b'))
    );
    assert_eq!(
        e.feed(k('b'), Mode::Insert),
        Feed::Cmd(Command::InsertChar('a'))
    );
}

#[test]
fn lang_stage_inert_outside_the_three_contexts() {
    // Acceptance #2 ("and to nothing else"): Normal/Replace never translate — operators and motions
    // are immune. Map `d`->`w`; in Normal `d` still ARMS the delete operator (Pending), it is NOT
    // rewritten to the `w` motion.
    let mut e = InputEngine::new();
    e.lang_map.insert('d', 'w');
    e.lang_active = true;
    assert_eq!(
        e.translate_lang(k('d'), Mode::Normal).code,
        KeyCode::Char('d')
    );
    assert_eq!(e.feed(k('d'), Mode::Normal), Feed::Pending);
    assert_eq!(
        e.translate_lang(k('d'), Mode::Replace).code,
        KeyCode::Char('d')
    );
}

#[test]
fn lang_stage_translates_single_char_argument() {
    // Acceptance #2 (positive half): a command reading a single character (`f`/`r`) has its ARGUMENT
    // translated, regardless of how the command was reached.
    let mut e = InputEngine::new();
    e.lang_map.insert('a', 'x');
    e.lang_active = true;
    // `f a` finds the TRANSLATED char `x`.
    assert_eq!(e.feed(k('f'), Mode::Normal), Feed::Pending);
    assert_eq!(
        e.feed(k('a'), Mode::Normal),
        Feed::Cmd(Command::Move(
            1,
            Motion::FindChar {
                ch: 'x',
                forward: true,
                till: false,
                repeat: false
            }
        ))
    );
    // `r a` replaces with the TRANSLATED char `x`.
    assert_eq!(e.feed(k('r'), Mode::Normal), Feed::Pending);
    assert_eq!(
        e.feed(k('a'), Mode::Normal),
        Feed::Cmd(Command::ReplaceChar(1, 'x'))
    );
}

#[test]
fn lang_stage_off_by_default_and_toggled_by_ctrl_caret() {
    // The map is inert until activated (so `:lmap`-defining keystrokes are never rewritten), and
    // `i_CTRL-^` toggles it within Insert (RFC-0013).
    let mut e = InputEngine::new();
    e.lang_map.insert('a', 'б');
    // Off by default: `a` inserts `a`.
    assert_eq!(
        e.feed(k('a'), Mode::Insert),
        Feed::Cmd(Command::InsertChar('a'))
    );
    // CTRL-^ activates (a silent, non-inserting toggle).
    assert_eq!(e.feed(ctrl_caret(), Mode::Insert), Feed::Pending);
    assert_eq!(
        e.feed(k('a'), Mode::Insert),
        Feed::Cmd(Command::InsertChar('б'))
    );
    // CTRL-^ again deactivates.
    assert_eq!(e.feed(ctrl_caret(), Mode::Insert), Feed::Pending);
    assert_eq!(
        e.feed(k('a'), Mode::Insert),
        Feed::Cmd(Command::InsertChar('a'))
    );
}

#[test]
fn lang_stage_translates_on_the_command_line() {
    // A `/` line follows the Insert language state; a `:` line starts unmapped, and `c_CTRL-^` flips
    // the map for that line only (nvim v0.12.6, #810). A mapped key appends its TRANSLATION to the
    // owned line buffer (F-026 owns the buffer; the stage rewrites the key before it lands there).
    let mut e = InputEngine::new();
    e.lang_map.insert('a', 'б');
    e.lang_active = true;
    assert_eq!(e.feed(k('/'), Mode::Normal), Feed::Pending); // opens the search line
    let _ = e.feed(k('a'), Mode::Normal);
    assert_eq!(e.cmdline.as_ref().map(|c| c.buffer.as_str()), Some("б"));
    e.cmdline = None;
    assert_eq!(e.feed(k(':'), Mode::Normal), Feed::Pending);
    let _ = e.feed(k('a'), Mode::Normal);
    assert_eq!(e.cmdline.as_ref().map(|c| c.buffer.as_str()), Some("a"));
    assert_eq!(e.feed(ctrl_caret(), Mode::Normal), Feed::Pending);
    let _ = e.feed(k('a'), Mode::Normal);
    assert_eq!(e.cmdline.as_ref().map(|c| c.buffer.as_str()), Some("aб"));
    assert!(
        e.lang_active,
        "a `:` line's CTRL-^ leaves the Insert state alone"
    );
    // On a `/` line CTRL-^ is the Insert state too.
    e.cmdline = None;
    let _ = e.feed(k('/'), Mode::Normal);
    let _ = e.feed(ctrl_caret(), Mode::Normal);
    assert!(!e.lang_active);
}

#[test]
fn lang_map_maintenance_methods() {
    // `set_lang_mapping` / `clear_lang_mapping` (the `:lmap` / `:lunmap` back-ends).
    let mut e = InputEngine::new();
    e.set_lang_mapping('a', 'б');
    e.lang_active = true;
    assert_eq!(
        e.feed(k('a'), Mode::Insert),
        Feed::Cmd(Command::InsertChar('б'))
    );
    e.clear_lang_mapping('a');
    assert_eq!(
        e.feed(k('a'), Mode::Insert),
        Feed::Cmd(Command::InsertChar('a'))
    );
}

#[test]
fn parse_lmap_and_lunmap() {
    assert_eq!(
        parse_ex("lmap a б"),
        Ex::Lmap {
            lhs: 'a', rhs: 'б'
        }
    );
    assert_eq!(parse_ex("lunmap a"), Ex::Lunmap { lhs: 'a' });
    // A verb needs whitespace, both sides are single chars, and no trailing tokens — else Unknown.
    assert!(matches!(parse_ex("lmapx"), Ex::Unknown(_)));
    assert!(matches!(parse_ex("lmap a"), Ex::Unknown(_)));
    assert!(matches!(parse_ex("lmap ab cd"), Ex::Unknown(_)));
    assert!(matches!(parse_ex("lmap a b c"), Ex::Unknown(_)));
    assert!(matches!(parse_ex("lunmapx"), Ex::Unknown(_)));
}
