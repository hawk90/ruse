use super::{ctrl, feed, k, meta};
use crate::input::*;

#[test]
fn emacs_profile_is_non_modal() {
    // F-012 seam: the Emacs profile resolves global-map C- motions to commands and self-inserts a
    // printable key — regardless of the (Vim) mode passed, and without the modal grammar.
    let mut e = InputEngine::emacs();
    assert_eq!(
        e.feed(ctrl('f'), Mode::Insert),
        Feed::Cmd(Command::MoveRight)
    );
    assert_eq!(
        e.feed(ctrl('b'), Mode::Insert),
        Feed::Cmd(Command::MoveLeft)
    );
    assert_eq!(
        e.feed(ctrl('n'), Mode::Insert),
        Feed::Cmd(Command::MoveDown)
    );
    assert_eq!(e.feed(ctrl('p'), Mode::Insert), Feed::Cmd(Command::MoveUp));
    assert_eq!(
        e.feed(ctrl('a'), Mode::Insert),
        Feed::Cmd(Command::MoveLineStart)
    );
    assert_eq!(
        e.feed(ctrl('e'), Mode::Insert),
        Feed::Cmd(Command::MoveLineEnd)
    );
    // A printable key self-inserts (no Insert mode needed) — proof of "move + insert in one state".
    assert_eq!(
        e.feed(k('x'), Mode::Insert),
        Feed::Cmd(Command::InsertChar('x'))
    );
    // `d` is a literal char here, NOT the Vim delete operator (the modal grammar is not consulted).
    assert_eq!(
        e.feed(k('d'), Mode::Insert),
        Feed::Cmd(Command::InsertChar('d'))
    );
    // An unbound control key is inert (not the start of a Vim construct).
    assert_eq!(e.feed(ctrl('z'), Mode::Insert), Feed::Ignored);
}

/// Feed `seq` through a NATIVE-profile engine (F-013), mirroring the Vim `feed` helper.
fn feed_native(seq: &str) -> Feed {
    let mut e = InputEngine::native();
    let mut last = Feed::Ignored;
    for c in seq.chars() {
        last = e.feed(k(c), Mode::Normal);
    }
    last
}

#[test]
fn native_profile_text_layer_is_the_vim_modal_grammar() {
    // F-013 NAT-1: the Native profile's TEXT layer REUSES the Vim operator/motion/text-object grammar.
    // In this slice that is the whole observable behaviour — Native drives text identically to Vim, so
    // every modal construct resolves to the same Command. (The additive leader/which-key and transient
    // layers land in later slices; they do not alter the text grammar asserted here.)
    for seq in [
        "w", "3w", "dw", "d2w", "2dw", "2d3w", "dd", "2dd", "dG", "dgg", "i", "x",
    ] {
        assert_eq!(
            feed_native(seq),
            feed(seq),
            "Native must resolve {seq:?} identically to Vim (NAT-1)"
        );
    }
}

#[test]
fn native_profile_is_modal_not_non_modal() {
    // The Native profile takes the MODAL path, NOT the Emacs non-modal one: an operator+motion composes
    // (`dw` = a delete over a word), and `C-f` is inert in Normal (a Vim construct start / unbound), NOT
    // the Emacs `MoveRight` self-resolving command. This pins Native to the Vim branch of `feed`.
    assert_eq!(
        feed_native("dw"),
        Feed::Cmd(Command::Delete(1, Motion::WordFwd))
    );
    let mut e = InputEngine::native();
    assert_ne!(
        e.feed(ctrl('f'), Mode::Normal),
        Feed::Cmd(Command::MoveRight)
    );
}

#[test]
fn native_leader_opens_whichkey_and_resolves_to_semantic_commands() {
    // F-013 NAT-2: `<leader>` (Space) from a clean Normal base ARMS the which-key tier (Pending, no
    // command yet); the next key resolves in the leader map to a SEMANTIC command (INV-CMD-SEMANTIC).
    let mut e = InputEngine::native();
    assert_eq!(e.feed(k(' '), Mode::Normal), Feed::Pending);
    assert!(e.leader_hint().is_some(), "Space must arm the leader tier");
    assert_eq!(e.feed(k('w'), Mode::Normal), Feed::Cmd(Command::Save));
    assert!(e.leader_hint().is_none(), "the selection disarms the tier");

    // Each seed binding resolves to its semantic command.
    for (key_char, cmd) in [
        ('w', Command::Save),
        ('q', Command::Quit),
        ('u', Command::Undo),
        ('r', Command::Redo),
    ] {
        let mut e = InputEngine::native();
        assert_eq!(e.feed(k(' '), Mode::Normal), Feed::Pending);
        assert_eq!(e.feed(k(key_char), Mode::Normal), Feed::Cmd(cmd));
    }

    // While armed, the discovery hint spells out the whole menu (the which-key render feeds on this).
    let e = {
        let mut e = InputEngine::native();
        e.feed(k(' '), Mode::Normal);
        e
    };
    assert_eq!(
        e.leader_hint().as_deref(),
        Some("w:write  q:quit  u:undo  r:redo")
    );
}

#[test]
fn native_leader_abort_and_gating() {
    // An unbound selection key is a which-key ABORT: the tier disarms and nothing fires.
    let mut e = InputEngine::native();
    assert_eq!(e.feed(k(' '), Mode::Normal), Feed::Pending);
    assert_eq!(e.feed(k('z'), Mode::Normal), Feed::Ignored);
    assert!(e.leader_hint().is_none());

    // The leader arms ONLY from a clean base: `d<Space>` is the Vim delete-right motion (NAT-1 intact),
    // NOT a leader — and it must equal what Vim does for the same keys.
    assert_eq!(feed_native("d "), feed("d "));
    assert!(InputEngine::native().leader_hint().is_none());

    // The leader is Native-ONLY: under Vim, Space keeps its Vim meaning (unbound here) and never arms a
    // which-key tier — the profile gate, not a shared keymap, is what makes the leader Native's alone.
    let mut v = InputEngine::new();
    assert_eq!(v.feed(k(' '), Mode::Normal), feed(" "));
    assert!(v.leader_hint().is_none());
}

#[test]
fn emacs_prefix_argument_multiplies_motions_and_repeats_text() {
    // F-012 / D-049: `C-u` seeds a prefix argument (default 4); digits make it explicit; a further
    // `C-u` multiplies by four. The next command consumes it OPAQUELY — a motion multiplies, a
    // self-insert repeats. Each accumulating key is Pending (consumed, not yet a command).
    let mut e = InputEngine::emacs();

    // Bare `C-u` → default 4: `C-u C-f` moves right four graphemes.
    assert_eq!(e.feed(ctrl('u'), Mode::Insert), Feed::Pending);
    assert_eq!(
        e.feed(ctrl('f'), Mode::Insert),
        Feed::Cmd(Command::Move(4, Motion::EmacsCharFwd))
    );

    // Explicit decimal: `C-u 3 7 C-b` moves left 37.
    assert_eq!(e.feed(ctrl('u'), Mode::Insert), Feed::Pending);
    assert_eq!(e.feed(k('3'), Mode::Insert), Feed::Pending);
    assert_eq!(e.feed(k('7'), Mode::Insert), Feed::Pending);
    assert_eq!(
        e.feed(ctrl('b'), Mode::Insert),
        Feed::Cmd(Command::Move(37, Motion::EmacsCharBack))
    );

    // `C-u C-u` → 16: a self-insert repeats sixteen times (Replay, not a single Cmd).
    assert_eq!(e.feed(ctrl('u'), Mode::Insert), Feed::Pending);
    assert_eq!(e.feed(ctrl('u'), Mode::Insert), Feed::Pending);
    assert_eq!(
        e.feed(k('a'), Mode::Insert),
        Feed::Repeat(Repeat {
            unit: vec![Command::InsertChar('a')],
            times: 16,
            ..Repeat::default()
        })
    );

    // count == 1 keeps the grapheme-aware bare motion — the no-argument path is unchanged.
    assert_eq!(
        e.feed(ctrl('f'), Mode::Insert),
        Feed::Cmd(Command::MoveRight)
    );
    // A bare digit with no pending argument is ordinary self-inserting text, not an argument.
    assert_eq!(
        e.feed(k('3'), Mode::Insert),
        Feed::Cmd(Command::InsertChar('3'))
    );
}

#[test]
fn emacs_cx_prefix_map_dispatches_second_key() {
    // F-012: `C-x` opens the extended-command prefix map — the first key is Pending, the second
    // resolves inside the map. `C-x C-s` saves, `C-x C-c` quits, `C-x u` undoes.
    let mut e = InputEngine::emacs();

    assert_eq!(e.feed(ctrl('x'), Mode::Insert), Feed::Pending);
    assert_eq!(e.feed(ctrl('s'), Mode::Insert), Feed::Cmd(Command::Save));

    assert_eq!(e.feed(ctrl('x'), Mode::Insert), Feed::Pending);
    // `C-x C-c` is the session's `save-buffers-kill-terminal`, which asks before quitting (#619).
    assert_eq!(
        e.feed(ctrl('c'), Mode::Insert),
        Feed::EmacsAction(crate::input::EmacsAction::KillTerminal)
    );

    assert_eq!(e.feed(ctrl('x'), Mode::Insert), Feed::Pending);
    assert_eq!(e.feed(k('u'), Mode::Insert), Feed::Cmd(Command::Undo));

    // An unbound second key (here `C-g`, keyboard-quit) cancels the prefix and is inert — and the engine
    // is not left wedged: the very next key dispatches normally in the global map.
    assert_eq!(e.feed(ctrl('x'), Mode::Insert), Feed::Pending);
    assert_eq!(e.feed(ctrl('g'), Mode::Insert), Feed::Ignored);
    assert_eq!(
        e.feed(ctrl('f'), Mode::Insert),
        Feed::Cmd(Command::MoveRight)
    );
    // A bare `C-s` with no prefix is unbound in the global map (not a save) — prefix scoping holds.
    assert_eq!(e.feed(ctrl('s'), Mode::Insert), Feed::Ignored);
}

#[test]
fn emacs_kill_and_yank_over_the_unnamed_register() {
    // F-012 / D-026: `C-k` kills into the unnamed register (the depth-1 kill ring) via `EmacsKillLine`
    // (D-051) — its own command, not Vim's `Delete(1, LineEnd)`, because at EOL it kills the newline.
    // `C-y` is Emacs yank (D-051, `EmacsYank`) — paste + set the mark, distinct from Vim `P`/`Paste`;
    // it honours a prefix count.
    let mut e = InputEngine::emacs();

    assert_eq!(
        e.feed(ctrl('k'), Mode::Insert),
        Feed::Cmd(Command::EmacsKillLine)
    );
    assert_eq!(
        e.feed(ctrl('y'), Mode::Insert),
        Feed::Cmd(Command::EmacsYank { count: 1 })
    );
    // `C-u 3 C-y` yanks three copies — the prefix argument is the yank's repeat count.
    assert_eq!(e.feed(ctrl('u'), Mode::Insert), Feed::Pending);
    assert_eq!(e.feed(k('3'), Mode::Insert), Feed::Pending);
    assert_eq!(
        e.feed(ctrl('y'), Mode::Insert),
        Feed::Cmd(Command::EmacsYank { count: 3 })
    );
}

#[test]
fn emacs_shift_tracked_only_for_non_char_keys() {
    let mut e = InputEngine::emacs();
    // Shift IS meaningful on a non-char key: C-S-<backspace> is kill-whole-line.
    let cs_bs = KeyEvent::new(
        KeyCode::Backspace,
        KeyModifiers::CONTROL | KeyModifiers::SHIFT,
    );
    assert_eq!(
        e.feed(cs_bs, Mode::Insert),
        Feed::Cmd(Command::EmacsKillWholeLine)
    );
    // A printable key that arrives WITH a Shift modifier still matches its unshifted binding — Shift is
    // folded into the char, so M-@ (incidental Shift) is still mark-word, not a lookup miss.
    let m_at_shift = KeyEvent::new(KeyCode::Char('@'), KeyModifiers::ALT | KeyModifiers::SHIFT);
    assert_eq!(
        e.feed(m_at_shift, Mode::Insert),
        Feed::Cmd(Command::EmacsMarkWord)
    );
    // Plain M-DEL (no shift) stays backward-kill-word — the new C-S-<backspace> does not shadow it.
    let m_del = KeyEvent::new(KeyCode::Backspace, KeyModifiers::ALT);
    assert_eq!(
        e.feed(m_del, Mode::Insert),
        Feed::Cmd(Command::EmacsBackwardKillWord { count: 1 })
    );
}

#[test]
fn emacs_esc_is_the_meta_prefix() {
    // #896: `ESC u` = `M-u` (Emacs 31.1 `esc-map`), wherever the read boundary falls.
    let mut e = InputEngine::emacs();
    let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
    assert_eq!(e.feed(esc, Mode::Insert), Feed::Pending);
    let want = InputEngine::emacs().feed(meta('u'), Mode::Insert);
    assert!(matches!(want, Feed::Cmd(_)), "M-u is bound: {want:?}");
    assert_eq!(e.feed(k('u'), Mode::Insert), want);
    // Consumed by one key: the next plain key inserts.
    assert_eq!(
        e.feed(k('u'), Mode::Insert),
        Feed::Cmd(Command::InsertChar('u'))
    );
    // A `C-u` count survives the prefix: `C-u 3 ESC d` = `C-u 3 M-d`.
    let mut e = InputEngine::emacs();
    let ctrl_u = KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL);
    e.feed(ctrl_u, Mode::Insert);
    e.feed(k('3'), Mode::Insert);
    e.feed(esc, Mode::Insert);
    let mut fresh = InputEngine::emacs();
    fresh.feed(ctrl_u, Mode::Insert);
    fresh.feed(k('3'), Mode::Insert);
    assert_eq!(
        e.feed(k('d'), Mode::Insert),
        fresh.feed(meta('d'), Mode::Insert)
    );
    // `C-x` owns its next key: `C-x ESC` does not arm the prefix.
    let mut e = InputEngine::emacs();
    let ctrl_x = KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL);
    e.feed(ctrl_x, Mode::Insert);
    assert_eq!(e.feed(esc, Mode::Insert), Feed::Ignored);
    assert_eq!(
        e.feed(k('u'), Mode::Insert),
        Feed::Cmd(Command::InsertChar('u'))
    );
}

#[test]
fn emacs_meta_tier_word_motions_and_buffer_ends() {
    // F-012: the Meta (`M-`, Alt) tier — word motions and buffer ends. Word motions honour the prefix
    // count; `M-<`/`M->` (buffer ends) ignore it. A plain `f` still self-inserts (no Alt, no C-).
    let mut e = InputEngine::emacs();

    // `M-f` (Emacs forward-word) rests point AFTER the word (between-char), so it uses EmacsWordFwd,
    // not Vim `w`/WordFwd (which jumps to the next word start).
    assert_eq!(
        e.feed(meta('f'), Mode::Insert),
        Feed::Cmd(Command::Move(1, Motion::EmacsWordFwd))
    );
    // `M-b` (Emacs backward-word) is the two-class mirror of `M-f` — it uses EmacsWordBack, not Vim `b`/
    // WordBack (which treats a punctuation run as its own word and `_` as a word char).
    assert_eq!(
        e.feed(meta('b'), Mode::Insert),
        Feed::Cmd(Command::Move(1, Motion::EmacsWordBack))
    );
    // `M-d` kills a word forward into the register (kill ring) — its own `EmacsKillWord` command (D-051),
    // distinct from Vim `Delete` so consecutive Emacs kills accumulate and Vim deletes never do.
    assert_eq!(
        e.feed(meta('d'), Mode::Insert),
        Feed::Cmd(Command::EmacsKillWord { count: 1 })
    );
    // Buffer ends: `M-<` to the absolute buffer start, `M->` to the absolute end — count-agnostic, and
    // each pushes the mark (D-051, `EmacsBufferEdge`), distinct from Vim `gg`/`G`.
    assert_eq!(
        e.feed(meta('<'), Mode::Insert),
        Feed::Cmd(Command::EmacsBufferEdge { start: true })
    );
    assert_eq!(
        e.feed(meta('>'), Mode::Insert),
        Feed::Cmd(Command::EmacsBufferEdge { start: false })
    );

    // The prefix argument multiplies a word motion: `C-u M-f` = forward four words.
    assert_eq!(e.feed(ctrl('u'), Mode::Insert), Feed::Pending);
    assert_eq!(
        e.feed(meta('f'), Mode::Insert),
        Feed::Cmd(Command::Move(4, Motion::EmacsWordFwd))
    );

    // A plain printable key (no Alt) still self-inserts — the Meta tier does not shadow text entry.
    assert_eq!(
        e.feed(k('f'), Mode::Insert),
        Feed::Cmd(Command::InsertChar('f'))
    );
    // An unbound Meta key is inert (not self-inserted).
    assert_eq!(e.feed(meta('z'), Mode::Insert), Feed::Ignored);
}

#[test]
fn emacs_essential_editing_keys() {
    // F-012: the editing keys that make the profile usable — RET / C-j newline, DEL / C-d delete,
    // C-/ and C-_ undo. Repeatable keys honour the prefix count via Replay.
    let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
    let backspace = KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE);
    let mut e = InputEngine::emacs();

    // RET and C-j both insert a newline.
    assert_eq!(
        e.feed(enter, Mode::Insert),
        Feed::Cmd(Command::InsertNewline)
    );
    assert_eq!(
        e.feed(ctrl('j'), Mode::Insert),
        Feed::Cmd(Command::InsertNewline)
    );
    // DEL deletes the char before point; C-d is Emacs `delete-char` — deletes forward without filling
    // the kill ring (D-026: DeleteForward, not the yanking Vim `x`/DeleteUnder), with a count.
    assert_eq!(
        e.feed(backspace, Mode::Insert),
        Feed::Cmd(Command::DeleteBack)
    );
    assert_eq!(
        e.feed(ctrl('d'), Mode::Insert),
        Feed::Cmd(Command::DeleteForward(1))
    );
    // C-/ and C-_ are undo.
    assert_eq!(e.feed(ctrl('/'), Mode::Insert), Feed::Cmd(Command::Undo));
    assert_eq!(e.feed(ctrl('_'), Mode::Insert), Feed::Cmd(Command::Undo));

    // Counts: `C-u 2 RET` inserts two newlines (Replay); `C-u 3 C-d` deletes three chars (native count).
    assert_eq!(e.feed(ctrl('u'), Mode::Insert), Feed::Pending);
    assert_eq!(e.feed(k('2'), Mode::Insert), Feed::Pending);
    assert_eq!(
        e.feed(enter, Mode::Insert),
        Feed::Repeat(Repeat {
            unit: vec![Command::InsertNewline],
            times: 2,
            ..Repeat::default()
        })
    );
    assert_eq!(e.feed(ctrl('u'), Mode::Insert), Feed::Pending);
    assert_eq!(e.feed(k('3'), Mode::Insert), Feed::Pending);
    assert_eq!(
        e.feed(ctrl('d'), Mode::Insert),
        Feed::Cmd(Command::DeleteForward(3))
    );
}

#[test]
fn emacs_mark_and_region_bindings() {
    // F-012 / D-027: C-SPC set-mark, C-w kill-region, M-w kill-ring-save, C-x C-x exchange-point-mark.
    let ctrl_space = KeyEvent::new(KeyCode::Char(' '), KeyModifiers::CONTROL);
    let mut e = InputEngine::emacs();

    assert_eq!(
        e.feed(ctrl_space, Mode::Insert),
        Feed::Cmd(Command::SetMark)
    );
    assert_eq!(
        e.feed(ctrl('w'), Mode::Insert),
        Feed::Cmd(Command::KillRegion)
    );
    assert_eq!(
        e.feed(meta('w'), Mode::Insert),
        Feed::Cmd(Command::CopyRegion)
    );
    // C-x C-x resolves inside the C-x prefix map (first key Pending).
    assert_eq!(e.feed(ctrl('x'), Mode::Insert), Feed::Pending);
    assert_eq!(
        e.feed(ctrl('x'), Mode::Insert),
        Feed::Cmd(Command::ExchangePointMark)
    );
}

#[test]
fn emacs_mx_reads_a_command_name_and_runs_it() {
    // F-012: M-x opens the minibuffer; typing a command name is Pending; <CR> resolves it via the
    // registry into a Command. An unknown name is inert.
    let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);

    let mut e = InputEngine::emacs();
    assert_eq!(e.feed(meta('x'), Mode::Insert), Feed::Pending);
    for c in "save-buffer".chars() {
        assert_eq!(e.feed(k(c), Mode::Insert), Feed::Pending);
    }
    assert_eq!(e.feed(enter, Mode::Insert), Feed::Cmd(Command::Save));

    // A named region command routes through M-x too.
    assert_eq!(e.feed(meta('x'), Mode::Insert), Feed::Pending);
    for c in "kill-region".chars() {
        assert_eq!(e.feed(k(c), Mode::Insert), Feed::Pending);
    }
    assert_eq!(e.feed(enter, Mode::Insert), Feed::Cmd(Command::KillRegion));

    // An unknown command name is a no-op (Emacs "[No match]").
    assert_eq!(e.feed(meta('x'), Mode::Insert), Feed::Pending);
    for c in "no-such-command".chars() {
        assert_eq!(e.feed(k(c), Mode::Insert), Feed::Pending);
    }
    assert_eq!(e.feed(enter, Mode::Insert), Feed::Ignored);
    // After the minibuffer closes, normal dispatch resumes.
    assert_eq!(
        e.feed(ctrl('f'), Mode::Insert),
        Feed::Cmd(Command::MoveRight)
    );
}

#[test]
fn emacs_profile_is_a_nine_tier_stack() {
    // F-012 / D-045: the Emacs profile is ONE nine-tier LayerStack (not Vim's separate sealed
    // namespaces), walked highest-priority first down to global-map. This locks the promotion so a
    // later edit cannot silently drop a tier or reorder the walk.
    let e = InputEngine::emacs();
    assert_eq!(e.emacs.map.depth(), 9);
    let order: Vec<&str> = e.emacs.map.order().collect();
    assert_eq!(
        order.first().copied(),
        Some("emacs.keymaptier.01.overriding-terminal-local-map")
    );
    assert_eq!(
        order.last().copied(),
        Some("emacs.keymaptier.09.global-map")
    );
    // global-map carries every binding; the eight upper tiers are transparent (no minor/major modes).
    assert_eq!(
        e.emacs
            .map
            .layer("emacs.keymaptier.09.global-map")
            .map(|l| l.is_empty()),
        Some(false)
    );
    assert_eq!(
        e.emacs
            .map
            .layer("emacs.keymaptier.01.overriding-terminal-local-map")
            .map(|l| l.is_empty()),
        Some(true)
    );
}
