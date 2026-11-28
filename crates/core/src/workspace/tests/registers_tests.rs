//! Registers and the OS clipboard.

use super::*;
use crate::register::RegKind;

/// `"+yiw` yanks the word into the SYSTEM clipboard (via the injected provider), and mirrors unnamed.
#[test]
fn clipboard_yank_word_reaches_the_provider() {
    let clip = MemClipboard::new();
    let mut w = Workspace::new(b"hello\nworld\n".to_vec());
    w.set_clipboard(Box::new(clip.clone()));
    w.apply(&Command::SetRegister(Some('+')));
    w.apply(&Command::Yank(1, Mo::InnerWord));
    assert_eq!(
        clip.contents().as_deref(),
        Some(&b"hello"[..]),
        "\"+yiw pushes the word to the OS clipboard"
    );
    assert_eq!(
        w.register_bytes(None),
        b"hello",
        "a clipboard yank still mirrors unnamed (Vim)"
    );
}

/// A plain (unregistered) yank must NEVER touch the clipboard provider.
#[test]
fn plain_yank_leaves_clipboard_untouched() {
    let clip = MemClipboard::new();
    let mut w = Workspace::new(b"hello\nworld\n".to_vec());
    w.set_clipboard(Box::new(clip.clone()));
    w.apply(&Command::Yank(1, Mo::InnerWord));
    assert!(
        clip.contents().is_none(),
        "an unnamed yank never writes the OS clipboard"
    );
}

/// `"+p` pastes FROM whatever an external app put on the clipboard (charwise, inline).
#[test]
fn clipboard_paste_reads_external_contents() {
    let clip = MemClipboard::new();
    clip.preload("PASTED");
    let mut w = Workspace::new(b"xy\n".to_vec());
    w.set_clipboard(Box::new(clip.clone()));
    w.apply(&Command::SetRegister(Some('+')));
    w.apply(&Command::Paste {
        after: true,
        count: 1,
        move_after: false,
    });
    assert_eq!(
        w.focused().doc.bytes(),
        b"xPASTEDy\n",
        "\"+p inserts the external clipboard text after the cursor"
    );
}

/// Linewise geometry survives the clipboard round-trip: `"+yy` then `"+p` opens a whole NEW line, even
/// though the OS clipboard only carries bytes (the mirror slot keeps its RegKind when bytes are unchanged).
#[test]
fn clipboard_linewise_round_trip_preserves_geometry() {
    let clip = MemClipboard::new();
    let mut w = Workspace::new(b"alpha\nbeta\n".to_vec());
    w.set_clipboard(Box::new(clip.clone()));
    w.apply(&Command::SetRegister(Some('+')));
    w.apply(&Command::Yank(1, Mo::Line)); // "+yy
    assert_eq!(clip.contents().as_deref(), Some(&b"alpha\n"[..]));
    w.apply(&Command::SetRegister(Some('+')));
    w.apply(&Command::Paste {
        after: true,
        count: 1,
        move_after: false,
    }); // "+p
    assert_eq!(
        w.focused().doc.bytes(),
        b"alpha\nalpha\nbeta\n",
        "a linewise clipboard register pastes as a new line, not inline"
    );
}

/// `"+dd` deletes into the clipboard and removes the line from the buffer.
#[test]
fn clipboard_delete_line_cuts_to_the_provider() {
    let clip = MemClipboard::new();
    let mut w = Workspace::new(b"alpha\nbeta\n".to_vec());
    w.set_clipboard(Box::new(clip.clone()));
    w.apply(&Command::SetRegister(Some('+')));
    w.apply(&Command::Delete(1, Mo::Line)); // "+dd
    assert_eq!(clip.contents().as_deref(), Some(&b"alpha\n"[..]));
    assert_eq!(w.focused().doc.bytes(), b"beta\n", "the line is cut out");
}

/// A clipboard double that RECORDS every `set` (so a test can tell "not written" from "written the same
/// text") and serves a fixed `get` — `None` models an unreadable clipboard (no reader, non-UTF-8, timeout).
#[derive(Clone, Default)]
struct RecordingClipboard {
    external: Option<String>,
    sets: std::sync::Arc<std::sync::Mutex<Vec<Vec<u8>>>>,
}

impl RecordingClipboard {
    fn reading(external: Option<&str>) -> RecordingClipboard {
        RecordingClipboard {
            external: external.map(str::to_string),
            ..RecordingClipboard::default()
        }
    }

    fn sets(&self) -> Vec<Vec<u8>> {
        self.sets.lock().expect("clipboard mutex").clone()
    }
}

impl crate::Clipboard for RecordingClipboard {
    fn get(&self) -> Option<String> {
        self.external.clone()
    }
    fn set(&self, bytes: &[u8]) {
        self.sets
            .lock()
            .expect("clipboard mutex")
            .push(bytes.to_vec());
    }
}

fn paste_after() -> Command {
    Command::Paste {
        after: true,
        count: 1,
        move_after: false,
    }
}

/// #755: `"+p` reads the clipboard and never writes it (a put sets no register in Vim), so the provider
/// sees no `set` — the external app keeps ownership and any rich data it put there.
#[test]
fn clipboard_paste_never_writes_the_provider() {
    let clip = RecordingClipboard::reading(Some("from app"));
    let mut w = Workspace::new(b"xy\n".to_vec());
    w.set_clipboard(Box::new(clip.clone()));
    w.apply(&Command::SetRegister(Some('+')));
    w.apply(&paste_after());
    assert_eq!(
        w.focused().doc.bytes(),
        b"xfrom appy\n",
        "the paste still lands"
    );
    assert_eq!(
        clip.sets(),
        Vec::<Vec<u8>>::new(),
        "\"+p must not write the OS clipboard"
    );
}

/// #755: with an unreadable clipboard, `"*p` pastes the session's mirror but must not push that stale
/// mirror over whatever the OS clipboard holds.
#[test]
fn clipboard_paste_with_unreadable_clipboard_does_not_push_the_stale_mirror() {
    let clip = RecordingClipboard::reading(None);
    let mut w = Workspace::new(b"hello\n".to_vec());
    w.set_clipboard(Box::new(clip.clone()));
    w.apply(&Command::SetRegister(Some('+')));
    w.apply(&Command::Yank(1, Mo::InnerWord)); // "+yiw
    assert_eq!(clip.sets(), vec![b"hello".to_vec()]);
    w.apply(&Command::SetRegister(Some('*')));
    w.apply(&paste_after()); // "*p
    assert_eq!(
        w.focused().doc.bytes(),
        b"hhelloello\n",
        "the mirror is what pastes"
    );
    assert_eq!(
        clip.sets(),
        vec![b"hello".to_vec()],
        "the paste adds no write"
    );
}

/// #755: `i_CTRL-R +` inserts from the clipboard without writing it.
#[test]
fn insert_register_from_clipboard_never_writes_the_provider() {
    let clip = RecordingClipboard::reading(Some("abc"));
    let mut w = Workspace::new(b"\n".to_vec());
    w.set_clipboard(Box::new(clip.clone()));
    w.apply(&Command::InsertRegister('+'));
    assert_eq!(w.focused().doc.bytes(), b"abc\n");
    assert_eq!(clip.sets(), Vec::<Vec<u8>>::new());
}

/// A clipboard yank writes every time, even of the bytes already there (Vim re-owns the selection).
#[test]
fn clipboard_yank_writes_each_time() {
    let clip = RecordingClipboard::reading(Some("hello"));
    let mut w = Workspace::new(b"hello\n".to_vec());
    w.set_clipboard(Box::new(clip.clone()));
    for _ in 0..2 {
        w.apply(&Command::SetRegister(Some('+')));
        w.apply(&Command::Yank(1, Mo::InnerWord));
    }
    assert_eq!(clip.sets(), vec![b"hello".to_vec(), b"hello".to_vec()]);
}

/// #1736: a `"+` yank of non-UTF-8 bytes reaches the provider byte for byte, not lossily replaced.
#[test]
fn clipboard_yank_of_non_utf8_bytes_reaches_the_provider_unaltered() {
    let line = b"caf\xe9 \xff\xfe ok\n";
    let clip = RecordingClipboard::reading(None);
    let mut w = Workspace::new(line.to_vec());
    w.set_clipboard(Box::new(clip.clone()));
    w.yank_lines_into(crate::SubRange::CurrentLine, Some('+')); // :y +
    assert_eq!(w.register_bytes(Some('+')), line);
    assert_eq!(clip.sets(), vec![line.to_vec()]);
    // A pending-register yank (`"+y$`) goes through the same push.
    w.apply(&Command::SetRegister(Some('+')));
    w.apply(&Command::Yank(1, Mo::LineEnd));
    assert_eq!(clip.sets().get(1), Some(&line[..line.len() - 1].to_vec()));
}

/// `:y +` (an Ex yank, not a pending-register command) writes the clipboard too; an unnamed `:y` does not.
#[test]
fn ex_yank_into_clipboard_writes_the_provider() {
    let clip = RecordingClipboard::reading(None);
    let mut w = Workspace::new(b"alpha\nbeta\n".to_vec());
    w.set_clipboard(Box::new(clip.clone()));
    w.yank_lines_into(crate::SubRange::CurrentLine, Some('+'));
    assert_eq!(clip.sets(), vec![b"alpha\n".to_vec()]);
    w.yank_lines_into(crate::SubRange::CurrentLine, None);
    assert_eq!(
        clip.sets().len(),
        1,
        "an unnamed :y leaves the clipboard alone"
    );
}

/// Graceful degradation: with the default [`NoClipboard`], `"+p` pastes nothing and never panics.
#[test]
fn clipboard_absent_is_a_graceful_noop() {
    let mut w = Workspace::new(b"hi\n".to_vec());
    w.apply(&Command::SetRegister(Some('+')));
    w.apply(&Command::Paste {
        after: true,
        count: 1,
        move_after: false,
    });
    assert_eq!(
        w.focused().doc.bytes(),
        b"hi\n",
        "no clipboard tool -> \"+p is inert, not a crash"
    );
}

/// #944: registers are editor-global (Vim, D-057 slice 2): `yy` in buffer A, then `p` in buffer B
/// pastes the line, and `"ayy` in A reaches `"ap` in B.
#[test]
fn a_yank_in_one_buffer_pastes_in_another() {
    let mut w = Workspace::new(b"from a\n".to_vec());
    w.apply(&Command::Yank(1, Mo::Line));
    w.apply(&Command::SetRegister(Some('a')));
    w.apply(&Command::Yank(1, Mo::Line));
    let b = w.add_buffer(b"b\n".to_vec(), Some("b".into()));
    assert!(w.focus_buffer(b));
    w.apply(&PASTE);
    w.apply(&Command::SetRegister(Some('a')));
    w.apply(&PASTE);
    assert_eq!(w.focused().doc.bytes(), b"b\nfrom a\nfrom a\n");
    assert_eq!(w.register_bytes(None), b"from a\n");
}

/// #944: a split shares the one store, so a yank in one pane pastes in the other.
#[test]
fn a_yank_in_one_split_pastes_in_the_other() {
    let mut w = Workspace::new(b"one\ntwo\n".to_vec());
    w.split(SplitDir::Horizontal);
    w.apply(&Command::Yank(1, Mo::Line));
    w.focus_next();
    w.apply(&Command::Move(1, Mo::Down));
    w.apply(&PASTE);
    assert_eq!(w.focused().doc.bytes(), b"one\ntwo\none\n");
    assert_eq!(w.registers().unnamed().text(), b"one\n");
}

/// #1022: `"#p` puts the alternate file name the frontend synced; a write naming a read-only register
/// fails and leaves the buffer and registers alone.
#[test]
fn hash_register_reads_the_alternate_and_read_only_writes_fail() {
    let mut w = Workspace::new(b"ab\n".to_vec());
    w.set_alternate_register(Some("other.txt".into()));
    w.apply(&Command::SetRegister(Some('#')));
    w.apply(&PASTE);
    assert_eq!(w.focused().doc.bytes(), b"aother.txtb\n");
    w.apply(&Command::SetRegister(Some('/')));
    let effects = w.apply(&Command::Delete(1, Mo::Line));
    assert!(effects.contains(&Effect::Beep), "\"/dd fails");
    assert_eq!(w.focused().doc.bytes(), b"aother.txtb\n");
    assert!(!w.registers().unnamed().is_set());
    // #1184: refused by its register, which the frontend tells from a failed motion.
    assert!(effects.contains(&Effect::RegisterRefused));
    assert!(w.take_beep() && w.take_refused());
    assert!(!w.take_refused(), "taking clears it");
    // `"/dfz` with no `z`: the motion fails first, so the operator is never refused.
    w.apply(&Command::SetRegister(Some('/')));
    let effects = w.apply(&Command::Delete(
        1,
        Mo::FindChar {
            ch: 'z',
            forward: true,
            till: false,
            repeat: false,
        },
    ));
    assert_eq!(effects, vec![Effect::Beep]);
    assert!(w.take_beep() && !w.take_refused());
}

/// D-055: raw macro bytes round-trip through a named register — `set_register_raw` then `register_bytes`
/// return the same key stream, and it shares the a-z slots with yank/paste (unset reads empty).
#[test]
fn macro_register_bytes_round_trip() {
    let mut w = Workspace::new(b"hello\n".to_vec());
    assert_eq!(w.register_bytes(Some('a')), b"", "unset register is empty");
    w.set_register_raw(Some('a'), b"iZ\x1b".to_vec()); // a macro: insert Z, escape
    assert_eq!(w.register_bytes(Some('a')), b"iZ\x1b");
    // A different register is independent.
    assert_eq!(w.register_bytes(Some('b')), b"");
}

/// F-029: `register_snapshot` lists only the NON-EMPTY registers, in `"`, `0`, a..z order.
#[test]
fn register_snapshot_lists_non_empty_in_order() {
    let mut w = Workspace::new(b"hello\n".to_vec());
    assert!(w.register_snapshot().is_empty(), "nothing set yet");
    w.set_register_raw(Some('b'), b"dd".to_vec());
    w.set_register_raw(Some('a'), b"iZ\x1b".to_vec());
    let snap = w.register_snapshot();
    assert_eq!(
        snap,
        vec![
            ('a', RegKind::Charwise, b"iZ\x1b".to_vec()),
            ('b', RegKind::Charwise, b"dd".to_vec()),
        ],
        "a before b; empty slots omitted; raw-set registers are charwise",
    );
}
