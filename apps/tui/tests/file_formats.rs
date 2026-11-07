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
    assert_eq!(h.bytes(), b"\xEF\xBB\xBFa\r\nb");
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
