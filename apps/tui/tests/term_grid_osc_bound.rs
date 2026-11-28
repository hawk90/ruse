//! An unterminated OSC from the child cannot grow memory without bound (#1779). This binary counts live
//! heap bytes with a wrapping global allocator, so it holds this one test only: a parallel test would
//! allocate into the same count.

#![cfg(unix)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use ruse_tui::term_grid::{ChunkParser, Grid};

struct Counting;

static LIVE: AtomicUsize = AtomicUsize::new(0);

// SAFETY: every call is forwarded to `System` unchanged; the count is a side table.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        LIVE.fetch_add(layout.size(), Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        LIVE.fetch_add(new_size, Ordering::Relaxed);
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOC: Counting = Counting;

/// 64 MiB of OSC body with no terminator, fed in PTY-sized pieces, keeps the heap growth near the 1 MiB cap;
/// the BEL after it ends the OSC and the text after that prints.
#[test]
fn a_huge_unterminated_osc_stays_bounded_and_parsing_resumes() {
    let mut grid = Grid::new(2, 8);
    let mut parser = ChunkParser::new();
    let piece = vec![b'A'; 64 * 1024];
    let before = LIVE.load(Ordering::Relaxed);
    parser.advance(&mut grid, b"\x1b]52;c;");
    for _ in 0..1024 {
        parser.advance(&mut grid, &piece);
    }
    let grown = LIVE.load(Ordering::Relaxed).saturating_sub(before);
    assert!(
        grown <= 4 << 20,
        "64 MiB of OSC body grew the heap by {grown} bytes"
    );
    parser.advance(&mut grid, b"\x07ok\x1b]0;t\x1b\\!");
    let row: String = (0..8)
        .filter_map(|c| {
            grid.cell(0, c)
                .map(|(t, _)| if t.is_empty() { " " } else { t })
        })
        .collect();
    assert_eq!(row, "ok!     ");
    assert_eq!(grid.cursor(), (0, 3));
}
