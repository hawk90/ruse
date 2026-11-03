//! Raw input decoding through a real PTY (#896): the bytes a terminal sends, split across reads the way a
//! slow link or a big paste splits them, decoded by the real binary. Results are read from disk.
//!
//! Each chunk passed to `raw_chunks` reaches the editor as its own read. Expected bytes are nvim v0.12.6's
//! (Vim profile) or Emacs 31.1's (Emacs profile) for the same input, except where a test says otherwise.

#![cfg(unix)]

mod support;

use std::path::Path;

use support::{Editor, Scratch};

/// Start on `f.txt` holding `start` with `env`, feed `chunks`, then quit with `quit`; return the file.
fn run(env: &[&str], start: &[u8], chunks: &[&[u8]], quit: &[u8]) -> Vec<u8> {
    let dir = Scratch::new();
    let file = dir.file("f.txt", start);
    let mut ed = Editor::start_env(&dir, &[Path::new("f.txt")], env);
    ed.raw_chunks(chunks);
    ed.raw_chunks(&[quit]);
    assert_eq!(ed.exit_status(), 0, "exits cleanly after {chunks:?}");
    std::fs::read(&file).unwrap()
}

fn vim(start: &[u8], chunks: &[&[u8]]) -> Vec<u8> {
    run(&[], start, chunks, b":wq\r")
}

/// `C-x C-s` then `C-x C-c`.
const EMACS_SAVE_QUIT: &[u8] = b"\x18\x13\x18\x03";

fn emacs(start: &[u8], chunks: &[&[u8]]) -> Vec<u8> {
    run(&["RUSE_PROFILE=emacs"], start, chunks, EMACS_SAVE_QUIT)
}

#[test]
fn a_csi_key_split_across_reads_decodes_as_one_key() {
    // Down arrow (`CSI B`, and the SS3 form) cut after its introducer, then `x`.
    for down in [&b"\x1b["[..], b"\x1bO"] {
        assert_eq!(vim(b"abc\ndef\n", &[down, b"Bx"]), b"abc\nef\n", "{down:?}");
    }
    // Normal-mode <Del> (`CSI 3 ~`, bound since #605) cut inside its parameters.
    assert_eq!(vim(b"abc\n", &[b"\x1b[3", b"~"]), b"bc\n");
    // A cut inside the parameters of a modified arrow (Shift-Down, `CSI 1 ; 2 B`).
    assert_eq!(vim(b"abc\ndef\n", &[b"\x1b[1;", b"2B", b"x"]), b"abc\nef\n");
}

#[test]
fn a_utf8_char_split_across_reads_is_one_char() {
    // `한` is ED 95 9C. ruse waits for the rest of the character; nvim gives up after its timeout, so the
    // expected bytes here are the typed text, not nvim's.
    assert_eq!(
        vim(b"abc\n", &[b"i\xed", b"\x95\x9c", b"\x1b"]),
        "한abc\n".as_bytes()
    );
}

#[test]
fn vim_alt_key_is_esc_then_the_key() {
    // `ESC 0` in one read decodes as Alt-0; nvim treats an unmapped `<M-0>` as `<Esc>0`.
    assert_eq!(
        vim(b"abc\n", &[b"iX", b"\x1b0", b"iY", b"\x1b"]),
        b"YXabc\n"
    );
    // The common case: `<Esc>:w` typed fast enough to share a read must not insert a `:`.
    let dir = Scratch::new();
    let file = dir.file("f.txt", b"abc\n");
    let mut ed = Editor::start(&dir, &[Path::new("f.txt")]);
    ed.raw_chunks(&[b"iX", b"\x1b:wq\r"]);
    assert_eq!(ed.exit_status(), 0);
    assert_eq!(std::fs::read(&file).unwrap(), b"Xabc\n");
}

#[test]
fn emacs_meta_key_in_one_read() {
    // `M-u` upcase-word, `M-d` kill-word.
    assert_eq!(emacs(b"one two\n", &[b"\x1bu"]), b"ONE two\n");
    assert_eq!(emacs(b"one two\n", &[b"\x1bd"]), b" two\n");
}

#[test]
fn emacs_esc_is_the_meta_prefix_across_reads() {
    // Emacs 31.1 binds `ESC u` to upcase-word: ESC is a prefix, so the key may come in a later read.
    assert_eq!(emacs(b"one two\n", &[b"\x1b", b"u"]), b"ONE two\n");
    assert_eq!(emacs(b"one two\n", &[b"\x1b", b"d", b"\x1bu"]), b" TWO\n");
    // `ESC x` is M-x: the minibuffer reads a command name.
    assert_eq!(
        emacs(b"one two\n", &[b"\x1b", b"x", b"upcase-word\r"]),
        b"ONE two\n"
    );
    // The prefix is consumed by one key: plain text after it inserts again.
    assert_eq!(emacs(b"ab\n", &[b"\x1b", b"f", b"Z"]), b"abZ\n");
}

const KITTY: &[&str] = &["TERM=xterm-kitty"];

#[test]
fn kitty_protocol_is_pushed_and_its_keys_decode() {
    let dir = Scratch::new();
    let file = dir.file("f.txt", b"abc\n");
    let mut ed = Editor::start_env(&dir, &[Path::new("f.txt")], KITTY);
    assert!(
        ed.screen_bytes().contains("\u{1b}[>3u"),
        "TERM=xterm-kitty pushes the kitty flags"
    );
    // Esc arrives as `CSI 27 u`; a key release (`:3`) is not a second keypress.
    ed.raw_chunks(&[b"ix\x1b[120;1:3u", b"\x1b[27u", b":wq\r"]);
    assert_eq!(ed.exit_status(), 0);
    assert_eq!(std::fs::read(&file).unwrap(), b"xabc\n");
    ed.settle();
    assert!(ed.screen_bytes().contains("\u{1b}[<u"), "pops them on exit");
}

#[test]
fn kitty_alt_key_is_esc_then_the_key() {
    // Alt-0 as `CSI 48 ; 3 u`.
    assert_eq!(
        run(
            KITTY,
            b"abc\n",
            &[b"iX", b"\x1b[48;3u", b"iY\x1b[27u"],
            b":wq\r"
        ),
        b"YXabc\n"
    );
}

/// #772: modifyOtherKeys (`CSI > 4 ; 2 m`) is never requested, under xterm or tmux, because its Ctrl keys
/// (`CSI 27 ; 5 ; 117 ~`, which tmux 3.8 sends with `extended-keys on`) cannot be decoded.
#[test]
fn modify_other_keys_is_never_requested() {
    for term in ["TERM=xterm-256color", "TERM=tmux-256color", "TERM=xterm"] {
        let dir = Scratch::new();
        dir.file("f.txt", b"abc\n");
        let mut ed = Editor::start_env(&dir, &[Path::new("f.txt")], &[term]);
        ed.raw_chunks(&[b":q\r"]);
        assert_eq!(ed.exit_status(), 0, "{term}");
        ed.settle();
        assert!(!ed.screen_bytes().contains("\u{1b}[>4;"), "{term}");
    }
}

/// Ctrl keys in the `CSI code ; mods u` form (kitty, or tmux with `extended-keys-format csi-u`) decode on
/// the command line and in Insert mode: `C-u` clears the line, `C-w` deletes a word.
#[test]
fn csi_u_ctrl_keys_decode_without_the_kitty_push() {
    assert_eq!(vim(b"x\n", &[b":abc", b"\x1b[117;5u", b"s/x/y/\r"]), b"y\n");
    assert_eq!(
        vim(b"x\n", &[b"ione two", b"\x1b[119;5u", b"Z", b"\x1b"]),
        b"one Zx\n"
    );
}

/// The xterm form `CSI 27 ; mods ; code ~` cannot be decoded and is dropped, but only that key: the next
/// keys still arrive (TUX-2, no input breakage after a keyboard-protocol failure).
#[test]
fn an_undecodable_modify_other_keys_key_drops_only_itself() {
    assert_eq!(
        vim(b"x\n", &[b":abc", b"\x1b[27;5;117~", b"\x15s/x/y/\r"]),
        b"y\n"
    );
    assert_eq!(
        vim(b"x\n", &[b"ione", b"\x1b[27;5;119~", b"Z", b"\x1b"]),
        b"oneZx\n"
    );
}

/// A bracketed paste of `body`, as one byte string.
fn paste(body: &[u8]) -> Vec<u8> {
    [&b"\x1b[200~"[..], body, b"\x1b[201~"].concat()
}

// Bracketed paste (#600). The paste is decoded as ONE unit however it is chunked, so none of its bytes run
// as keys (the payloads would delete the line or quit), its text reaches the buffer, and the key after it is
// a key. A Normal-mode paste is `a{text}<Esc>` (nvim): after a payload ending in a newline the cursor rests
// on the next line's first char, which the trailing `x` deletes. CRLF and a lone CR become one newline.

#[test]
fn paste_in_one_read_runs_none_of_its_bytes() {
    let p = paste(b"dd:q!\r");
    assert_eq!(vim(b"abc\n", &[&p, b"x"]), b"add:q!\nc\n");
}

#[test]
fn paste_with_split_start_and_end_codes() {
    // Cut inside `CSI 200 ~`, inside the body, and inside `CSI 201 ~`.
    let chunks: &[&[u8]] = &[b"\x1b[20", b"0~dd:q!", b"\r\x1b[2", b"01", b"~x"];
    assert_eq!(vim(b"abc\n", chunks), b"add:q!\nc\n");
}

#[test]
fn paste_across_a_64_byte_boundary_with_crlf() {
    // The first read ends 64 bytes in, between CR and LF: the pair is still one newline.
    let mut first = b"\x1b[200~".to_vec();
    first.resize(63, b'd');
    first.push(b'\r');
    assert_eq!(first.len(), 64);
    let body = "d".repeat(63 - 6);
    assert_eq!(
        vim(b"abc\n", &[&first, b"\ndd\r\n\x1b[201~x"]),
        format!("a{body}\ndd\nc\n").as_bytes()
    );
}

#[test]
fn a_large_paste_burst_is_one_unit() {
    // 24 KiB: many decoder reads (crossterm reads 1 KiB at a time) and more than one PTY buffer.
    let n = 24 * 1024 / 7;
    let p = paste(&b"dd:q!\r\n".repeat(n));
    let mut want = b"a".to_vec();
    want.extend_from_slice(&b"dd:q!\n".repeat(n));
    want.extend_from_slice(b"c\n");
    assert_eq!(vim(b"abc\n", &[&p, b"x"]), want);
}

#[test]
fn paste_in_insert_mode_inserts_the_text() {
    // nvim inserts `PP` before the caret; the `Esc` after the paste still leaves Insert.
    let p = paste(b"PP");
    assert_eq!(vim(b"abc\n", &[b"i", &p, b"\x1b"]), b"PPabc\n");
}

#[test]
fn paste_start_cut_after_its_esc_is_not_a_paste_yet() {
    // KNOWN GAP (#629): with no `ttimeoutlen`, a read holding only the ESC of `CSI 200 ~` decodes as the
    // Esc key at once, so the rest of the start code and the payload run as keys (`~` upcases `a`, `PP`
    // puts an empty register). nvim waits `ttimeoutlen` (50 ms) for the rest and inserts `PP`. Terminals
    // write the start code in one write, so this takes a slow link cutting right after the ESC. When the
    // key timeout lands this becomes `aPPbc\n`.
    assert_eq!(vim(b"abc\n", &[b"\x1b", b"[200~PP\x1b[201~"]), b"Abc\n");
}

/// In a `:terminal` (the child is `/bin/sh`), run `cmd` writing `out.txt`, paste `body`, end the input with
/// `CTRL-D CTRL-D`, and return what the child wrote.
fn terminal_paste(cmd: &str, body: &[u8]) -> Vec<u8> {
    let dir = Scratch::new();
    let out = dir.path("out.txt");
    let mut ed = Editor::start(&dir, &[]);
    // A live terminal never goes quiet, so each settle runs to the harness's limit: two, not three.
    ed.keys(&format!(":terminal\r{cmd} > out.txt\r"));
    ed.raw_chunks(&[&paste(body), b"\x04\x04"]);
    let start = std::time::Instant::now();
    loop {
        let got = std::fs::read(&out).unwrap_or_default();
        if !got.is_empty() || start.elapsed() > std::time::Duration::from_secs(10) {
            return got;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

#[test]
fn terminal_mode_paste_goes_to_the_child_without_escapes() {
    // nvim's `terminal_paste`: ESC is dropped, lines end in `\n`, and no bracket unless the child asked.
    assert_eq!(
        terminal_paste("cat", b"one\x1b[31mX\r\ntwo\n"),
        b"one[31mX\ntwo\n"
    );
}

#[test]
fn terminal_paste_is_bracketed_when_the_child_enabled_it() {
    // The child turns on mode 2004 first; the paste then arrives wrapped (the wrapper is the only ESC).
    assert_eq!(
        terminal_paste("printf '\\033[?2004h'; cat", b"a\x1b[2Jb\n"),
        b"\x1b[200~a[2Jb\n\x1b[201~"
    );
}

#[test]
fn a_pasted_control_sequence_never_reaches_the_buffer() {
    // TERM-PASTE-2: ESC and C1 controls are dropped (nvim inserts them literally).
    let p = paste("i\u{1b}]52;c;eA==\u{7}\u{9b}2J".as_bytes());
    assert_eq!(vim(b"abc\n", &[&p]), "ai]52;c;eA==\u{7}2Jbc\n".as_bytes());
}

#[test]
fn emacs_paste_inserts_at_point() {
    // `C-x C-c` inside the payload is text, not save-buffers-kill-terminal.
    let p = paste(b"\x18\x03X\r\nY");
    assert_eq!(emacs(b"abc\n", &[&p]), b"\x18\x03X\nYabc\n");
}
