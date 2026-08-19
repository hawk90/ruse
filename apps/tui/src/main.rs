//! `ruse` — a terminal-based modal text editor. This binary is a one-line shim: the entire frontend lives
//! in the `ruse-tui` library (see `lib.rs`), so the editor is exercised as a library by the parity/bench
//! harnesses rather than re-compiled here. Keys → semantic commands → plan/commit → the core returns
//! Effects that the library performs; `ruse --replay <trace> <file>` reproduces a session without a terminal.

fn main() -> std::process::ExitCode {
    ruse_tui::run()
}
