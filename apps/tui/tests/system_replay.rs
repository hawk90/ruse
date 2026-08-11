//! SYSTEM test (testing-and-benchmarks.md §1.9 E2E / §1.10 deterministic replay): drive the REAL
//! compiled `ruse` binary end-to-end through its headless `--replay` entry (F-022). This is the only
//! layer that exercises the finished binary AS A PROCESS — argument parsing, trace/file IO, the core
//! replay pipeline, stdout, and exit codes — proving the determinism contract (D-001, DET-1..3) at the
//! process boundary rather than only in-library.
//!
//! Adversarial by construction: every failure path — missing arguments, unreadable inputs, a malformed
//! trace, and (the load-bearing one) a trace whose recorded base does NOT match the file it is replayed
//! onto — must exit FAILURE and say so on stderr, never silently emit a wrong document. A replay engine
//! that "succeeds" against the wrong base is the exact class of bug this contract exists to forbid.

use ruse_core::{Command, Trace};
use std::path::PathBuf;
use std::process::Command as Proc;
use std::sync::atomic::{AtomicU32, Ordering};

/// A unique scratch path per call (parallel-test safe): pid + a process-local counter.
fn scratch(name: &str) -> PathBuf {
    static N: AtomicU32 = AtomicU32::new(0);
    let mut d = std::env::temp_dir();
    d.push(format!(
        "ruse-systest-{}-{}-{name}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    d
}

/// The compiled binary under test (Cargo exports its path to integration tests).
fn ruse() -> Proc {
    Proc::new(env!("CARGO_BIN_EXE_ruse"))
}

/// Write `bytes` to a fresh scratch file and return its path.
fn file_with(name: &str, bytes: &[u8]) -> PathBuf {
    let p = scratch(name);
    std::fs::write(&p, bytes).expect("write scratch file");
    p
}

#[test]
fn replay_applies_trace_and_prints_the_resulting_document() {
    // " world" typed at end of line, then leave insert — the canonical determinism fixture.
    let mut cmds = vec![Command::MoveLineEnd, Command::EnterInsert];
    cmds.extend(" world".chars().map(Command::InsertChar));
    cmds.push(Command::EnterNormal);

    let file = file_with("doc.txt", b"hello");
    let trace = file_with(
        "t.trace",
        Trace::record(b"hello", cmds).to_text().as_bytes(),
    );

    let out = ruse()
        .arg("--replay")
        .arg(&trace)
        .arg(&file)
        .output()
        .expect("spawn ruse --replay");

    assert!(
        out.status.success(),
        "expected success; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        out.stdout, b"hello world",
        "replayed document must be byte-identical to the recorded run"
    );
}

#[test]
fn replay_is_deterministic_across_runs() {
    // DET-1: the SAME log against the SAME base reproduces the identical final bytes, run after run.
    let cmds = vec![
        Command::EnterInsert,
        Command::InsertChar('Z'),
        Command::EnterNormal,
    ];
    let file = file_with("det.txt", b"ab");
    let trace = file_with("det.trace", Trace::record(b"ab", cmds).to_text().as_bytes());

    let run = || {
        ruse()
            .arg("--replay")
            .arg(&trace)
            .arg(&file)
            .output()
            .expect("spawn")
            .stdout
    };
    let a = run();
    let b = run();
    assert_eq!(a, b, "replay must be deterministic");
    assert_eq!(a, b"Zab");
}

#[test]
fn replay_missing_arguments_fails_with_usage() {
    let out = ruse().arg("--replay").output().expect("spawn");
    assert!(!out.status.success(), "missing args must fail");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("usage"),
        "stderr should carry a usage line, got: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn replay_unreadable_inputs_fail() {
    let out = ruse()
        .arg("--replay")
        .arg(scratch("nope.trace")) // never created
        .arg(scratch("nope.txt"))
        .output()
        .expect("spawn");
    assert!(!out.status.success(), "unreadable inputs must fail");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("cannot read"),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn replay_malformed_trace_fails() {
    let file = file_with("m.txt", b"x");
    let trace = file_with("bad.trace", b"this is not a ruse-trace header\n");
    let out = ruse()
        .arg("--replay")
        .arg(&trace)
        .arg(&file)
        .output()
        .expect("spawn");
    assert!(!out.status.success(), "a malformed trace must fail");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("bad trace"),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn replay_base_mismatch_is_rejected_not_misapplied() {
    // The load-bearing guard: a trace recorded against "hello" must REFUSE to replay onto "goodbye"
    // (Trace::replay -> TraceError::HashMismatch), not run the commands against the wrong base and
    // print a plausible-but-wrong document. Adversarial: we assert BOTH the failure AND that stdout
    // did not carry a document.
    let trace_txt = Trace::record(b"hello", vec![Command::MoveLineEnd]).to_text();
    let trace = file_with("mismatch.trace", trace_txt.as_bytes());
    let file = file_with("other.txt", b"goodbye");

    let out = ruse()
        .arg("--replay")
        .arg(&trace)
        .arg(&file)
        .output()
        .expect("spawn");

    assert!(
        !out.status.success(),
        "base mismatch must fail, but exited success with stdout: {:?}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(
        out.stdout.is_empty(),
        "a rejected replay must emit no document, got: {:?}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("replay failed"),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}
