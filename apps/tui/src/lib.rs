//! Library face of the `ruse-tui` crate.
//!
//! The binary (`main.rs`) is the terminal frontend and owns all IO. This library face exists so the
//! pure, testable pieces — today the [`input`] engine that folds keystrokes into semantic
//! [`ruse_core::Command`]s — can be driven from integration tests WITHOUT a terminal. The parity
//! comparison harness (`tests/parity_compare.rs`) uses it to replay a fixture's keystrokes through
//! the real input engine + core and diff the result against the Neovim oracle.
//!
//! `main.rs` keeps its own `mod input;` and does not depend on this face, so exposing it here is
//! purely additive to the binary.

pub mod caps;
pub mod input;
