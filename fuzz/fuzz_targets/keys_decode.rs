//! Raw terminal input bytes through the key decoder (#920): any byte stream decodes without a panic, as
//! the `keys` never-panic proptest states (#1057).

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = ruse_tui::keys::decode(data);
});
