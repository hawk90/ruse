//! A hostile language server's byte stream through the LSP frame reader (#920), through
//! `ruse_tui::fuzz_entry`, which only `cargo fuzz` builds (the codec is crate-internal).

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    ruse_tui::fuzz_entry::lsp_frames(data);
});
