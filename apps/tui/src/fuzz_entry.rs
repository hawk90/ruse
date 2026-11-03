//! Entry points for the coverage-guided fuzz targets in `fuzz/` (#920). Compiled only under `cargo fuzz`,
//! which builds every crate with `--cfg fuzzing`, so the crate-internal parsers (the terminal grid, the
//! LSP framing) can be fuzzed without widening the public surface of a normal build. Each function runs one
//! input and panics only on a real defect: the never-panic proptests in those modules state the same
//! contracts over generated input.

/// Child-process output into the terminal grid, with a resize taken from the first two bytes; the cursor
/// stays on the screen (`term_grid::tests::hostile_output_never_panics`).
#[cfg(unix)]
pub fn term_grid(data: &[u8]) {
    let (size, rest) = data.split_at(data.len().min(2));
    let rows = u16::from(size.first().copied().unwrap_or(24) % 40) + 1;
    let cols = u16::from(size.get(1).copied().unwrap_or(80) % 120) + 1;
    let mut g = crate::term_grid::Grid::new(rows, cols);
    let mut parser = vte::Parser::new();
    for chunk in rest.split(|&b| b == 0xFF) {
        parser.advance(&mut g, chunk);
        let (r, c) = g.size();
        let (cy, cx) = g.cursor();
        assert!(cy < r && cx < c, "cursor ({cy},{cx}) outside {r}x{c}");
        if chunk.len() == 1 {
            // A one-byte chunk resizes, so resizes meet every parser state.
            g.resize(
                u16::from(chunk[0] % 30) + 1,
                u16::from(chunk[0] / 2 % 90) + 1,
            );
        }
    }
}

/// A hostile language server's byte stream through the LSP frame reader: every read ends or makes
/// progress (`lsp::codec::tests::read_frame_never_panics_and_terminates`).
pub fn lsp_frames(data: &[u8]) {
    let mut cur = std::io::Cursor::new(data);
    for _ in 0..=data.len() + 1 {
        let before = cur.position();
        match crate::lsp::codec::read_frame(&mut cur) {
            Ok(Some(_)) => assert!(cur.position() > before, "a frame read made no progress"),
            Ok(None) | Err(_) => return,
        }
    }
    panic!("the frame reader did not reach EOF");
}
