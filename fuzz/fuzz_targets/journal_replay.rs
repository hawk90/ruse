//! Recovery-journal bytes through the reader (#920): a torn, truncated or hostile journal is read as far
//! as its CRC-checked records allow and never panics (`journal` never-panic proptests, #1040, #1057).

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // Both journal kinds: a Checkpoint base needs no disk text; a DiskBase checks it against `disk`.
    let _ = ruse_tui::persist::rj01::replay(data, None);
    let _ = ruse_tui::persist::rj01::replay(data, Some(data));
});
