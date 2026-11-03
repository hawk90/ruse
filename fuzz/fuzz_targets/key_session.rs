//! Keystroke sessions through the real key path (`Headless`, #895), coverage-guided (#920): the same
//! contract as `apps/tui/tests/session_fuzz.rs` — after every key the buffer is valid UTF-8, the cursor on
//! a char boundary and the whole state passes the invariant walker (#906); a full undo then restores the
//! starting buffer.
//!
//! Input: the first byte picks how many pieces of hard text start the buffer, the next bytes pick them, and
//! every later byte is one key from a fixed alphabet. The alphabet has no `:`, `!` or `Z`, so a session
//! never reaches the Ex line, a shell or a write: the fuzzer cannot touch files or run commands. Count
//! digits come in runs of at most two, so one command cannot ask for gigabytes.

#![no_main]

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use libfuzzer_sys::fuzz_target;
use ruse_core::Command;
use ruse_tui::headless::Headless;

const HARD: &[&str] = &[
    "가",
    "漢字",
    "e\u{301}",
    "👨\u{200d}👩\u{200d}👧",
    "🇰🇷",
    "\t",
    "\n",
    "a",
    "b ",
    "foo.bar",
    "(x)",
    "\"q\"",
];

/// Upper bound on the buffer, as in `session_fuzz` (yank-all + paste doubles it per key).
const MAX_BUFFER: usize = 256 * 1024;

fn alphabet() -> Vec<KeyEvent> {
    let mut v = Vec::new();
    for c in "hjklwbeWBE0$^gGiaoOvVRxXpPJ~DCYsdcyfFtT;,nN/?\".123456789AIqm`'%{}()[]<>@uU*#rS é가\u{301}\t".chars() {
        v.push(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
    }
    for code in [
        KeyCode::Esc,
        KeyCode::Enter,
        KeyCode::Backspace,
        KeyCode::Left,
        KeyCode::Right,
    ] {
        v.push(KeyEvent::new(code, KeyModifiers::NONE));
    }
    for c in ['o', 'v', 'r', 'w', 'a', 'x', 'd', 'u', 'e', 'y'] {
        v.push(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL));
    }
    v
}

fn is_digit(k: &KeyEvent) -> bool {
    matches!(k.code, KeyCode::Char('0'..='9')) && k.modifiers == KeyModifiers::NONE
}

fuzz_target!(|data: &[u8]| {
    let Some((&n, rest)) = data.split_first() else {
        return;
    };
    let n = usize::from(n % 12).min(rest.len());
    let (pieces, keys) = rest.split_at(n);
    let init: String = pieces
        .iter()
        .map(|&b| HARD[usize::from(b) % HARD.len()])
        .collect();

    let abc = alphabet();
    let mut h = Headless::vim(&init);
    let mut digits = 0;
    for &b in keys.iter().take(400) {
        let key = abc[usize::from(b) % abc.len()];
        digits = if is_digit(&key) { digits + 1 } else { 0 };
        if digits > 2 {
            continue;
        }
        h.key(key);
        let ws = h.workspace();
        let (bytes, cursor) = (ws.focused().doc.bytes(), ws.focused().view.cursor());
        let s = std::str::from_utf8(bytes).expect("the buffer stays valid UTF-8");
        assert!(
            s.is_char_boundary(cursor),
            "cursor {cursor} off a char boundary"
        );
        ws.assert_invariants();
        if bytes.len() > MAX_BUFFER || h.quit() {
            break;
        }
    }
    h.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    let ws = h.workspace_mut();
    for _ in 0..100_000 {
        if !ws.focused().doc.can_undo() {
            break;
        }
        ws.apply(&Command::Undo);
    }
    // A session that left the starting buffer (`CTRL-^`, a split onto another) undoes the one it is on.
    if ws.focused_buffer() == ruse_core::DocumentId(1) {
        assert_eq!(
            ws.focused().doc.bytes(),
            init.as_bytes(),
            "a full undo restores the buffer"
        );
    }
});
