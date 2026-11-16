use super::*;

/// #1069: a C0 punctuation chord matches in both encodings — the kitty protocol's `CTRL-]` and the
/// `CTRL-5` crossterm decodes from a legacy terminal's 0x1d — and a letter chord only as itself.
#[test]
fn is_ctrl_accepts_the_legacy_encoding_of_punctuation_chords() {
    let ctrl = |c| KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL);
    for (c, legacy) in [('\\', '4'), (']', '5'), ('^', '6'), ('_', '7'), ('@', ' ')] {
        assert!(is_ctrl(ctrl(c), c), "CTRL-{c}");
        assert!(is_ctrl(ctrl(legacy), c), "CTRL-{c} sent as CTRL-{legacy}");
        assert!(!is_ctrl(
            KeyEvent::new(KeyCode::Char(legacy), KeyModifiers::NONE),
            c
        ));
    }
    assert!(is_ctrl(ctrl('w'), 'w'));
    assert!(!is_ctrl(ctrl('5'), 'w'));
    assert!(!is_ctrl(
        KeyEvent::new(KeyCode::Char(']'), KeyModifiers::NONE),
        ']'
    ));
}

/// Run one ex line against `ws` through the session's Ex dispatch, returning `(status, quit)`.
pub(super) fn ex_run(ws: &mut Workspace, line: &str) -> (String, bool) {
    let s = crate::app::session::Session::ex_on(ws, line);
    (s.ed.status.to_string(), s.ed.quit)
}
