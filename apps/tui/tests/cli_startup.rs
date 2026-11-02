//! Startup and command-line matrix of the real binary (#896): flags, the `--` separator, and what happens
//! without a terminal. Exit statuses: 0 for `--help`/`--version`, 2 for a usage error, 1 for a run that
//! cannot start.

#![cfg(unix)]

mod support;

use std::path::Path;
use std::process::Stdio;

use support::{open_pty, run_detached, Editor, Scratch};

#[test]
fn help_prints_usage_and_succeeds() {
    for flag in ["--help", "-h"] {
        let out = run_detached(&[flag], Stdio::null(), Stdio::piped());
        assert_eq!(out.status, 0, "{flag}");
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(text.starts_with("usage: ruse"), "{flag}: {text:?}");
        assert!(out.stderr.is_empty(), "{flag}: {:?}", out.stderr);
    }
}

#[test]
fn version_prints_the_package_version() {
    for flag in ["--version", "-V"] {
        let out = run_detached(&[flag], Stdio::null(), Stdio::piped());
        assert_eq!(out.status, 0, "{flag}");
        assert_eq!(
            String::from_utf8_lossy(&out.stdout),
            format!("ruse {}\n", env!("CARGO_PKG_VERSION"))
        );
    }
}

#[test]
fn unknown_option_is_a_usage_error() {
    // nvim v0.12.6 prints `Unknown option argument: "-x"` and exits 1; ruse uses the conventional usage
    // status 2 so a script can tell a bad command line from a failed run.
    for args in [&["-x"][..], &["--bogus"], &["-"], &["f.txt", "-x"]] {
        let out = run_detached(args, Stdio::null(), Stdio::piped());
        assert_eq!(out.status, 2, "{args:?}");
        assert!(out.stdout.is_empty(), "{args:?}");
        let first = out.stderr.lines().next().unwrap_or_default();
        assert!(
            first.starts_with("ruse: unknown option: -"),
            "{args:?}: {first:?}"
        );
        assert!(out.stderr.contains("ruse --help"), "{args:?}");
    }
}

#[test]
fn several_files_are_refused_rather_than_dropped() {
    let out = run_detached(&["a.txt", "b.txt"], Stdio::null(), Stdio::piped());
    assert_eq!(out.status, 2);
    assert!(
        out.stderr.starts_with("ruse: 2 files given"),
        "{:?}",
        out.stderr
    );
}

#[test]
fn without_a_terminal_it_says_so_and_fails() {
    // stdin /dev/null with piped stdout used to fail in raw mode with a bare OS error ("Device not
    // configured"). nvim waits on the non-terminal; ruse cannot draw there, so it stops at once.
    let out = run_detached(&["f.txt"], Stdio::null(), Stdio::piped());
    assert_eq!(out.status, 1);
    assert_eq!(out.stderr, "ruse: standard input is not a terminal\n");
    assert!(out.stdout.is_empty(), "nothing is drawn into a pipe");

    // stdin a pipe.
    let out = run_detached(&[], Stdio::piped(), Stdio::piped());
    assert_eq!(out.status, 1);
    assert_eq!(out.stderr, "ruse: standard input is not a terminal\n");

    // A terminal on stdin only: stdout is a pipe.
    let (_master, slave) = open_pty();
    let out = run_detached(&[], Stdio::from(slave), Stdio::piped());
    assert_eq!(out.status, 1);
    assert_eq!(out.stderr, "ruse: standard output is not a terminal\n");
    assert!(out.stdout.is_empty());
}

#[test]
fn double_dash_opens_a_file_named_like_an_option() {
    let dir = Scratch::new();
    let file = dir.file("-x", b"dash\n");
    let mut ed = Editor::start(&dir, &[Path::new("--"), Path::new("-x")]);
    ed.keys("A!\u{1b}:wq\r");
    assert_eq!(ed.exit_status(), 0);
    assert_eq!(std::fs::read(&file).unwrap(), b"dash!\n");
}

#[test]
fn double_dash_makes_a_subcommand_name_a_file() {
    let dir = Scratch::new();
    let file = dir.path("agent");
    let mut ed = Editor::start(&dir, &[Path::new("--"), Path::new("agent")]);
    ed.keys("ihi\u{1b}:wq\r");
    assert_eq!(ed.exit_status(), 0);
    assert!(std::fs::read(&file).unwrap().starts_with(b"hi"));
}

#[test]
fn no_file_opens_an_unnamed_buffer_on_a_terminal() {
    let dir = Scratch::new();
    let mut ed = Editor::start(&dir, &[]);
    assert!(ed.is_running());
    ed.keys(":q\r");
    assert_eq!(ed.exit_status(), 0);
}
