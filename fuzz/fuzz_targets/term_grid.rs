//! Untrusted child-process output into the `:terminal` grid (#920), through `ruse_tui::fuzz_entry`, which
//! only `cargo fuzz` builds (the grid is crate-internal).

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    ruse_tui::fuzz_entry::term_grid(data);
});
