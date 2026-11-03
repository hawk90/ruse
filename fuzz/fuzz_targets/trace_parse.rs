//! Replay-trace text through the parser (#920): any text is a trace or a typed error, never a panic
//! (`trace::tests::from_text_never_panics_on_arbitrary_text`, #1057). A parsed trace also replays on a
//! small buffer without a panic; a replay is the same command stream a session produces.

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let text = String::from_utf8_lossy(data);
    if let Ok(trace) = ruse_core::Trace::from_text(&text) {
        let _ = trace.replay(b"ab\ncd\n");
    }
});
