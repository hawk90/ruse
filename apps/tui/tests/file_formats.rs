//! Line endings, `'binary'` and large files (#619), against nvim v0.12.6 (`nvim --clean --headless` on the
//! same files, the bytes read back with `od`) and Emacs 31.1 (`emacs -Q --batch`).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use ruse_tui::headless::{Headless, Profile};

fn scratch(name: &str, bytes: &[u8]) -> PathBuf {
    static N: AtomicU32 = AtomicU32::new(0);
    let d = std::env::temp_dir().join(format!(
        "ruse-formats-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&d).unwrap();
    let f = std::path::absolute(d.join(name)).unwrap();
    std::fs::write(&f, bytes).unwrap();
    f
}

fn vim(f: &Path) -> Headless {
    Headless::visiting(Profile::Vim, f)
}

fn read(f: &Path) -> Vec<u8> {
    std::fs::read(f).unwrap()
}

#[test]
fn fileformat_follows_fileformats() {
    let unix = scratch("u.txt", b"a\nb\n");
    let dos = scratch("w.txt", b"a\r\nb\r\n");
    let mac = scratch("m.txt", b"a\rb\r");
    assert_eq!(
        vim(&unix).keys(":set ff?<CR>").status(),
        "  fileformat=unix"
    );
    assert_eq!(vim(&dos).keys(":set ff?<CR>").status(), "  fileformat=dos");
    // The default `unix,dos` reads a CR-only file as one unix line; with `mac` listed it is two mac lines.
    let mut h = vim(&mac);
    assert_eq!(h.keys(":set ff?<CR>").status(), "  fileformat=unix");
    h.keys(":set ffs=unix,dos,mac<CR>:e!<CR>");
    assert_eq!(h.keys(":set ff?<CR>").status(), "  fileformat=mac");
    assert_eq!(h.text(), "a\nb\n");
    let mut h = vim(&mac);
    h.keys(":e ++ff=mac<CR>");
    assert_eq!(h.text(), "a\nb\n");
    assert_eq!(
        vim(&unix).keys(":set ffs?<CR>").status(),
        "  fileformats=unix,dos"
    );
}

#[test]
fn setting_fileformat_converts_the_file_on_write() {
    let f = scratch("u.txt", b"a\nb\n");
    let mut h = vim(&f);
    h.keys(":set ff=dos<CR>");
    assert_eq!(h.keys(":set mod?<CR>").status(), "  modified");
    h.keys(":w<CR>");
    assert_eq!(
        h.status(),
        format!("\"{}\" [dos] 2L, 6B written", f.display())
    );
    assert_eq!(read(&f), b"a\r\nb\r\n");
    h.keys(":set ff=mac<CR>:w<CR>");
    assert_eq!(
        h.status(),
        format!("\"{}\" [mac] 2L, 4B written", f.display())
    );
    assert_eq!(read(&f), b"a\rb\r");
    assert_eq!(
        h.keys(":set ff=foo<CR>").status(),
        "E474: Invalid argument: ff=foo"
    );
    assert_eq!(
        h.keys(":set ffs=unix,foo<CR>").status(),
        "E474: Invalid argument: ffs=unix,foo"
    );
}

#[test]
fn binary_zeroes_the_formatting_options_and_restores_them() {
    let mut h = Headless::vim("a\n");
    h.keys(":set tw=20 wm=3 et<CR>:set bin<CR>");
    assert_eq!(h.keys(":set bin?<CR>").status(), "  binary");
    assert_eq!(h.keys(":set tw?<CR>").status(), "  textwidth=0");
    assert_eq!(h.keys(":set wm?<CR>").status(), "  wrapmargin=0");
    assert_eq!(h.keys(":set et?<CR>").status(), "noexpandtab");
    h.keys(":set nobin<CR>");
    assert_eq!(h.keys(":set tw?<CR>").status(), "  textwidth=20");
    assert_eq!(h.keys(":set wm?<CR>").status(), "  wrapmargin=3");
    assert_eq!(h.keys(":set et?<CR>").status(), "  expandtab");
}

#[test]
fn binary_reads_and_writes_the_bytes_as_they_are() {
    // nvim: `:e ++bin` keeps the BOM and the CR as text, and `x` + `:w` writes the rest back unconverted.
    let f = scratch("b.txt", b"\xEF\xBB\xBFa\r\nb");
    let mut h = vim(&f);
    h.keys(":e ++bin<CR>");
    assert_eq!(h.keys(":set bin?<CR>").status(), "  binary");
    // The buffer is line-terminated (RFC-0024); the missing final newline is `'endofline'`.
    assert_eq!(h.bytes(), b"\xEF\xBB\xBFa\r\nb\n");
    // `x` deletes the BOM, which is one character, as in nvim.
    h.keys("gg0x:w<CR>");
    assert_eq!(read(&f), b"a\r\nb");
    // `:set bin` after a read: the write converts nothing (no CRLF, no BOM) and keeps the missing newline,
    // even under 'fixeol'.
    let f = scratch("c.txt", b"\xEF\xBB\xBFa\r\nb");
    let mut h = vim(&f);
    h.keys(":set fixeol bin<CR>x:w<CR>");
    assert_eq!(read(&f), b"\nb");
    assert!(
        h.status().contains("[noeol] 2L, 2B written"),
        "{}",
        h.status()
    );
}

/// Emacs 31.1: `File big.txt is large (10 MiB), really open? ([Y]es, [N]o, [L]iterally, [?]): `; `n` is
/// `Aborted`, `l` opens it literally (no line-ending conversion).
#[test]
fn emacs_large_file_warning() {
    let big = scratch("big.txt", &b"a\r\n".repeat(11_000_000 / 3 + 1));
    let small = scratch("f.txt", b"x\n");
    let mut h = Headless::visiting(Profile::Emacs, &small);
    let find = |h: &mut Headless| {
        h.keys("<C-x><C-f>");
        let back = "<BS>".repeat(h.status().len() - "Find file: ".len());
        h.keys(&format!("{back}{}<CR>", big.display()));
    };
    find(&mut h);
    assert_eq!(
        h.status(),
        "File big.txt is large (10 MiB), really open? ([Y]es, [N]o, [L]iterally, [?]): "
    );
    h.keys("n");
    assert_eq!(h.status(), "Aborted");
    assert_eq!(h.text(), "x\n");
    find(&mut h);
    h.keys("l");
    assert!(h.text().starts_with("a\r\na\r\n"), "literally: CR kept");
}

/// #1298, RFC-0024: a file without a final newline reads line-terminated, so `x` on the only character of
/// its last line (`\nh`) leaves two empty lines, as nvim v0.12.6 (`'nofixeol'`): the cursor stays on the
/// second, `k` and `G` move between them, and `:w` writes `\n` (one newline joins the two lines).
#[test]
fn emptying_the_last_line_of_a_file_without_a_final_newline_keeps_the_line() {
    let f = scratch("e.txt", b"\nh");
    let mut h = vim(&f);
    h.keys("Gx");
    assert_eq!(h.bytes(), b"\n\n");
    assert_eq!(h.cursor_row_col(), (2, 0), "1-based: the second line");
    h.keys("g<C-g>");
    assert_eq!(
        h.status(),
        "Col 1 of 0; Line 2 of 2; Word 0 of 0; Char 2 of 2; Byte 2 of 1"
    );
    assert_eq!(h.keys("k").cursor_row_col(), (1, 0));
    assert_eq!(h.keys("G").cursor_row_col(), (2, 0));
    h.keys(":w<CR>");
    assert_eq!(read(&f), b"\n");
    assert!(
        h.status().contains("[noeol] 2L, 1B written"),
        "{}",
        h.status()
    );
}

/// RFC-0024: `'endofline'` is the file's final newline, whatever the buffer text: `:set noeol` on a file
/// that had one drops it on the next write, and `:set eol` adds it back (nvim v0.12.6, `'nofixeol'`).
#[test]
fn set_noeol_drops_the_final_newline_on_the_next_write() {
    let f = scratch("n.txt", b"a\n");
    let mut h = vim(&f);
    h.keys(":set noeol<CR>:w<CR>");
    assert_eq!(read(&f), b"a");
    h.keys(":set eol<CR>:w<CR>");
    assert_eq!(read(&f), b"a\n");
}

/// RFC-0024 §3: text that leaves a buffer without `'endofline'` ends as the file does. nvim v0.12.6
/// (`'nofixeol'`) on `a\nb`: `:1w` writes the whole line `a\n`, `:2w` writes `b`, `:w !cat` and the
/// filter `:%!cat` send three bytes, so the filter gives the same text back.
#[test]
fn ranges_pipes_and_filters_end_as_the_file_does() {
    let f = scratch("r.txt", b"a\nb");
    let first = scratch("first.txt", b"");
    let last = scratch("last.txt", b"");
    let mut h = vim(&f);
    h.keys(&format!(":1w! {}<CR>", first.display()));
    assert_eq!(read(&first), b"a\n");
    h.keys(&format!(":2w! {}<CR>", last.display()));
    assert_eq!(read(&last), b"b");
    h.keys(":w !wc -c<CR>");
    assert!(h.status().trim_start().starts_with('3'), "{}", h.status());
    h.keys(":%!cat<CR>:w<CR>");
    assert_eq!(read(&f), b"a\nb");
    // What the filter reads, counted: three bytes, for `:%!` and for `gq` through 'formatprg'.
    let mut h = vim(&f);
    h.keys(":%!wc -c<CR>");
    assert_eq!(String::from_utf8_lossy(&h.bytes()).trim(), "3");
    let mut h = vim(&f);
    h.keys(":set formatprg=wc\\ -c<CR>gqG");
    assert_eq!(String::from_utf8_lossy(&h.bytes()).trim(), "3");
}

/// A read whose format is forced reads `'endofline'` anew (RFC-0024): `:e ++ff=mac` of a classic Mac file
/// that `fileformats=unix,dos` first read as unix keeps its last CR, and `++bin` of a DOS file ending in
/// CTRL-Z sees no final newline after it; an unedited `:w` writes each back byte for byte.
#[test]
fn a_forced_read_format_keeps_the_files_final_line_break() {
    let f = scratch("m.txt", b"a\rb\r");
    let mut h = vim(&f);
    h.keys(":e ++ff=mac<CR>");
    assert_eq!(h.bytes(), b"a\nb\n");
    h.keys(":w!<CR>");
    assert_eq!(read(&f), b"a\rb\r");
    let f = scratch("z.txt", b"a\r\n\x1a");
    let mut h = vim(&f);
    h.keys(":e ++bin<CR>");
    assert_eq!(h.bytes(), b"a\r\n\x1a\n");
    h.keys(":w!<CR>");
    assert_eq!(read(&f), b"a\r\n\x1a");
}

/// #730, RFC-0024 §5: the emptied buffer (`ggdG`, nvim's `ML_EMPTY`) has one empty line, and a command that
/// adds lines keeps it. nvim v0.12.6 (`'nofixeol'`) writes these bytes from `a\nb\n`; a command that changes
/// nothing leaves the buffer emptied (it writes nothing), and one `u` goes back to it.
#[test]
fn the_emptied_buffer_keeps_its_line_for_commands_that_add_lines() {
    for (keys, want) in [
        ("ggdGo", &b"\n\n"[..]),
        ("ggdGO", b"\n\n"),
        ("ggdGoa", b"\na\n"),
        ("ggdGia", b"a\n"),
        ("yyggdGp", b"\na\nb\n"),
        ("yyggdGP", b"a\nb\n\n"),
        ("yyggdG:put<CR>", b"\na\nb\n"),
        ("yyggdG:0put<CR>", b"a\nb\n\n"),
        ("ggdG:r OTHER<CR>", b"\nr1\nr2\n"),
        ("ggdG:0r OTHER<CR>", b"r1\nr2\n\n"),
        ("ggdGi<Esc>", b""),
        ("ggdGo<Esc>u", b""),
        ("ggdGi<CR><Esc>", b"\n\n"),
        ("ggdGiab<Esc>U", b"\n"),
    ] {
        let f = scratch("e.txt", b"a\nb\n");
        std::fs::write(f.with_file_name("OTHER"), b"r1\nr2\n").unwrap();
        let mut h = vim(&f);
        h.keys(":cd ");
        h.keys(&format!("{}<CR>", f.parent().unwrap().display()));
        h.keys(keys);
        h.keys("<Esc>:w!<CR>");
        assert_eq!(read(&f), want, "{keys}");
    }
}

/// RFC-0024 §5: on a `'nomodifiable'` emptied buffer `o` fails without making its line (nvim v0.12.6: E21,
/// not modified).
#[test]
fn a_nomodifiable_emptied_buffer_stays_unmodified() {
    let mut h = Headless::vim("");
    h.keys(":set nomodifiable<CR>o<Esc>");
    assert_eq!(h.text(), "");
    assert_eq!(h.keys(":set modified?<CR>").status(), "nomodified");
}

/// RFC-0024 §4: the last line's newline matches `\n` in `:s`, but no substitution removes it (nvim v0.12.6).
#[test]
fn substitute_matches_the_last_line_break_and_keeps_it() {
    for (text, cmd, want) in [
        ("a\nb\nc\n", ":%s/\\n//<CR>", "abc\n"),
        ("a\nb\nc\n", ":%s/\\n/-/<CR>", "a-b-c-\n"),
        ("a\nb\n", ":2s/\\n/-/<CR>", "a\nb-\n"),
        ("a\nb\n", ":2s/\\n//<CR>", "a\nb\n"),
    ] {
        let mut h = Headless::vim(text);
        h.keys(cmd);
        assert_eq!(h.text(), want, "{text:?} {cmd}");
    }
}
