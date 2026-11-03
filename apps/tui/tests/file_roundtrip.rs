//! Byte round trip of open and write over a committed corpus (#909): every file in
//! `tests/fixtures/files/` (line-ending mixes, BOMs, no final newline, lone and trailing CR, NUL, invalid
//! UTF-8, UTF-16 and Latin-1 bytes, wide, combining and ZWJ text, a long line, control bytes) is opened by
//! the real binary on a PTY and written unedited under a new name, and the written bytes must equal the
//! file's. Neovim v0.12.6 writes every one of them back unchanged with `'nofixeol'`, which is the
//! byte-preserving default ruse keeps (#1107); with its default `'fixeol'` it adds a final newline to the
//! three files that lack one. The pure half (detect, strip, restore) is a property in
//! `persist/encoding.rs`.

#![cfg(unix)]

mod support;

use std::path::Path;
use support::{Editor, Scratch};

fn corpus() -> Vec<(String, Vec<u8>)> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/files");
    let mut files: Vec<(String, Vec<u8>)> = std::fs::read_dir(&dir)
        .expect("the fixture directory exists")
        .map(|e| {
            let p = e.expect("a directory entry").path();
            let name = p
                .file_name()
                .expect("a name")
                .to_string_lossy()
                .into_owned();
            (name, std::fs::read(&p).expect("a readable fixture"))
        })
        .collect();
    files.sort();
    files
}

#[test]
fn opening_and_writing_a_file_keeps_its_bytes() {
    let files = corpus();
    assert!(files.len() >= 20, "the corpus is in place");
    let dir = Scratch::new();
    for (name, bytes) in &files {
        dir.file(name, bytes);
    }
    let first = Path::new(&files[0].0);
    let mut ed = Editor::start(&dir, &[first]);
    let mut bad = Vec::new();
    for (name, bytes) in &files {
        let out = format!("out-{name}");
        ed.keys(&format!(":e {name}\r:w {out}\r"));
        let got = support::wait_for_file(&dir.path(&out), bytes);
        if &got != bytes {
            bad.push(format!("{name}: wrote {got:?}, want {bytes:?}"));
        }
    }
    ed.keys(":qa!\r");
    assert_eq!(ed.exit_status(), 0);
    assert!(
        bad.is_empty(),
        "round trip changed bytes:\n{}",
        bad.join("\n")
    );
}
