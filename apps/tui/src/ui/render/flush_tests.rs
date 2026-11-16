//! The diff flush and the synchronized-output fence. Split from `render.rs` (#1386).

use super::*;
use crossterm::style::Color;

/// F-006 #1: an unchanged frame emits ZERO bytes — no full-screen redraw.
#[test]
fn an_unchanged_frame_emits_nothing() {
    let mut a = screen::Screen::new(20, 3);
    a.put_str(0, 0, "hello world", Color::Reset, false);
    let mut b = screen::Screen::new(20, 3);
    b.put_str(0, 0, "hello world", Color::Reset, false);
    let mut buf: Vec<u8> = Vec::new();
    flush_diff(&mut buf, &b, &a, false).unwrap();
    assert!(buf.is_empty(), "identical frames must produce no output");
}

/// F-006 #1: a one-cell change emits a small, bounded batch containing just the new glyph — not
/// the whole screen. (A full redraw of 60 cells would be far larger.)
#[test]
fn a_one_cell_change_emits_only_that_cell() {
    let mut a = screen::Screen::new(20, 3);
    a.put_str(0, 0, "hello world", Color::Reset, false);
    let mut b = screen::Screen::new(20, 3);
    b.put_str(0, 0, "hello wOrld", Color::Reset, false); // one char differs
    let mut buf: Vec<u8> = Vec::new();
    flush_diff(&mut buf, &b, &a, false).unwrap();
    let s = String::from_utf8_lossy(&buf);
    assert!(s.contains('O'), "the changed glyph is emitted");
    assert!(!s.contains("hello"), "the unchanged run is NOT re-emitted");
}

/// F-006 #3: with sync support the batch is fenced in DEC synchronized output (?2026h/l).
#[test]
fn sync_output_fences_the_batch_when_supported() {
    let a = screen::Screen::new(10, 1);
    let mut b = screen::Screen::new(10, 1);
    b.put(0, 0, "Z", Color::Reset, false);
    let mut on: Vec<u8> = Vec::new();
    flush_diff(&mut on, &b, &a, true).unwrap();
    assert!(
        on.windows(8).any(|w| w == b"\x1b[?2026h"),
        "begins synchronized output"
    );
    assert!(
        on.windows(8).any(|w| w == b"\x1b[?2026l"),
        "ends synchronized output"
    );
    let mut off: Vec<u8> = Vec::new();
    flush_diff(&mut off, &b, &a, false).unwrap();
    assert!(
        !off.windows(8).any(|w| w == b"\x1b[?2026h"),
        "no fence when unsupported"
    );
}
