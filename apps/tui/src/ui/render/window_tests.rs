//! Window painting: faces, conceal, folds, virtual blocks, images, grapheme clusters. Split from `render.rs` (#1386).

use super::*;

/// The faces channel reaches the cell grid: a byte carrying a bold / italic face paints a bold /
/// italic cell, and a selection's reverse-video merges ON TOP of the face rather than replacing it
/// (F-031 slice 0 — the decoration model's first attribute).
#[test]
fn paint_pane_carries_bold_and_italic_faces() {
    let bytes = b"ab";
    let styles = [
        screen::CellStyle {
            bold: true,
            ..screen::CellStyle::default()
        },
        screen::CellStyle {
            italic: true,
            ..screen::CellStyle::default()
        },
    ];
    let rect = Rect {
        x: 0,
        y: 0,
        w: 8,
        h: 2,
    };
    let mut cur = screen::Screen::new(8, 2);
    paint_pane(
        &mut cur,
        rect,
        bytes,
        &styles,
        0,
        None,
        None,
        &[],
        &[],
        &[],
        0,
        &[],
        &mut Vec::new(),
        false,
        &std::collections::HashMap::new(),
        &[],
        TAB_WIDTH,
    );
    assert!(cur.cell(0, 0).style.bold, "byte 0 paints bold");
    assert!(cur.cell(0, 1).style.italic, "byte 1 paints italic");

    let mut sel = screen::Screen::new(8, 2);
    paint_pane(
        &mut sel,
        rect,
        bytes,
        &styles,
        0,
        Some((0, 1)),
        None,
        &[],
        &[],
        &[],
        0,
        &[],
        &mut Vec::new(),
        false,
        &std::collections::HashMap::new(),
        &[],
        TAB_WIDTH,
    );
    let c = sel.cell(0, 0);
    assert!(
        c.style.bold && c.style.reverse,
        "selection reverse merges with the bold face",
    );
}

/// F-031 conceal (slice 1): a concealed range is HIDDEN (0 cells, following columns shift) on a
/// non-caret line, but REVEALED (painted normally) on the caret's own line so its markup stays
/// editable — the reveal-at-point rule (D-052). Selection/motion are unaffected (core untouched), so
/// P3/P4/P5 hold by construction.
#[test]
fn paint_pane_conceals_off_caret_line_and_reveals_on_it() {
    let bytes = b"abcd\nabcd"; // two identical lines
    let no_style: &[screen::CellStyle] = &[];
    let rect = Rect {
        x: 0,
        y: 0,
        w: 8,
        h: 2,
    };
    let conceal: [(usize, usize, Option<&str>); 2] = [(0, 2, None), (5, 7, None)]; // hide "ab" both lines

    // Caret on line 1: line 0's "ab" is concealed so 'c' shifts to col 0; line 1 is revealed so 'a'
    // stays at col 0.
    let mut s = screen::Screen::new(8, 2);
    paint_pane(
        &mut s,
        rect,
        bytes,
        no_style,
        0,
        None,
        None,
        &[],
        &[],
        &conceal,
        1,
        &[],
        &mut Vec::new(),
        false,
        &std::collections::HashMap::new(),
        &[],
        TAB_WIDTH,
    );
    assert_eq!(
        s.cell(0, 0).content,
        screen::Content::Cluster("c".into()),
        "off-caret line: concealed 'ab' shifts 'c' to col 0",
    );
    assert_eq!(
        s.cell(1, 0).content,
        screen::Content::Cluster("a".into()),
        "caret line revealed: 'a' stays at col 0",
    );

    // Caret on line 0: now line 0 is revealed, so 'a' stays at col 0 there too.
    let mut s2 = screen::Screen::new(8, 2);
    paint_pane(
        &mut s2,
        rect,
        bytes,
        no_style,
        0,
        None,
        None,
        &[],
        &[],
        &conceal,
        0,
        &[],
        &mut Vec::new(),
        false,
        &std::collections::HashMap::new(),
        &[],
        TAB_WIDTH,
    );
    assert_eq!(
        s2.cell(0, 0).content,
        screen::Content::Cluster("a".into()),
        "caret line revealed: 'a' stays at col 0",
    );
}

/// F-031 virt_text (slice 2): a concealed range carrying virt_text paints the GLYPH in the marker's
/// place on a non-caret line (`- x` → `• x`), and shows the real marker on the revealed caret line.
#[test]
fn paint_pane_virt_text_replaces_concealed_marker_off_caret_line() {
    let bytes = b"- x\n- y"; // two list items; conceal "- " [0,2)/[4,6), virt "• "
    let no_style: &[screen::CellStyle] = &[];
    let rect = Rect {
        x: 0,
        y: 0,
        w: 8,
        h: 2,
    };
    let conceal: [(usize, usize, Option<&str>); 2] = [(0, 2, Some("• ")), (4, 6, Some("• "))];

    // Caret on line 1: line 0 shows the bullet glyph then the text; line 1 (revealed) shows "- y".
    let mut s = screen::Screen::new(8, 2);
    paint_pane(
        &mut s,
        rect,
        bytes,
        no_style,
        0,
        None,
        None,
        &[],
        &[],
        &conceal,
        1,
        &[],
        &mut Vec::new(),
        false,
        &std::collections::HashMap::new(),
        &[],
        TAB_WIDTH,
    );
    assert_eq!(
        s.cell(0, 0).content,
        screen::Content::Cluster("•".into()),
        "off-caret line: '- ' concealed, '•' painted in its place",
    );
    assert_eq!(
        s.cell(0, 2).content,
        screen::Content::Cluster("x".into()),
        "text follows the 2-cell bullet glyph",
    );
    assert_eq!(
        s.cell(1, 0).content,
        screen::Content::Cluster("-".into()),
        "caret line revealed: raw '-' marker shows",
    );
}

/// Manual folds: a closed fold collapses its interior lines — its START row shows the summary and the
/// line below the fold paints on the very next display row.
#[test]
fn paint_pane_collapses_a_closed_fold() {
    let bytes = b"a\nb\nc\nd\ne"; // lines 0..=4
    let no_style: &[screen::CellStyle] = &[];
    let rect = Rect {
        x: 0,
        y: 0,
        w: 20,
        h: 5,
    };
    // Fold lines 1..=3 closed: line 1 shows the summary, lines 2/3 hidden, line 4 paints on row 2.
    let folds = [crate::folds::Fold {
        start: 1,
        end: 3,
        closed: true,
        level: 1,
    }];
    let mut s = screen::Screen::new(20, 5);
    paint_pane(
        &mut s,
        rect,
        bytes,
        no_style,
        0,
        None,
        None,
        &[],
        &[],
        &[],
        9,
        &[],
        &mut Vec::new(),
        false,
        &std::collections::HashMap::new(),
        &folds,
        TAB_WIDTH,
    );
    assert_eq!(
        s.cell(0, 0).content,
        screen::Content::Cluster("a".into()),
        "line 0 on row 0",
    );
    assert_eq!(
        s.cell(1, 0).content,
        screen::Content::Cluster("\u{25b8}".into()),
        "fold summary (▸) on the fold's start row",
    );
    assert_eq!(
        s.cell(2, 0).content,
        screen::Content::Cluster("e".into()),
        "line 4 collapses up to row 2 (lines 2,3 hidden)",
    );
}

/// End-to-end `foldmethod=indent` (slice 3): the pure pipeline `compute_indent_folds` →
/// `apply_foldlevel` → `paint_pane` collapses a NESTED indent buffer. At foldlevel=0 the outermost fold
/// closes and hides its whole subtree behind one summary row (nvim-verified ranges).
#[test]
fn paint_pane_collapses_computed_indent_folds() {
    // 0:a0  1:·b  2:··c  3:··d  4:·e  5:f  (sw=4). Folds: outer [1,4] L1, inner [2,3] L2.
    let bytes = b"a0\n    b\n        c\n        d\n    e\nf";
    let mut folds = crate::folds::compute_indent_folds(bytes, 4, 4);
    crate::folds::apply_foldlevel(&mut folds, 0); // foldlevel=0 → close all outermost
    let no_style: &[screen::CellStyle] = &[];
    let rect = Rect {
        x: 0,
        y: 0,
        w: 20,
        h: 6,
    };
    let mut s = screen::Screen::new(20, 6);
    paint_pane(
        &mut s,
        rect,
        bytes,
        no_style,
        0,
        None,
        None,
        &[],
        &[],
        &[],
        9,
        &[],
        &mut Vec::new(),
        false,
        &std::collections::HashMap::new(),
        &folds,
        TAB_WIDTH,
    );
    assert_eq!(
        s.cell(0, 0).content,
        screen::Content::Cluster("a".into()),
        "the level-0 header line stays visible above the fold",
    );
    assert_eq!(
        s.cell(1, 0).content,
        screen::Content::Cluster("\u{25b8}".into()),
        "the outer fold's summary (▸) collapses the whole indented subtree onto its start row",
    );
    assert_eq!(
        s.cell(2, 0).content,
        screen::Content::Cluster("f".into()),
        "the trailing level-0 line pulls up to row 2 (lines 2..=4 hidden inside the fold)",
    );
}

/// F-031 slice 3a (row model): a virtual block pushes the buffer lines below it DOWN, and the block's
/// first row shows the placeholder label.
#[test]
fn paint_pane_places_lines_below_a_virtual_block() {
    let bytes = b"a\nb"; // line 0 "a", line 1 "b"
    let no_style: &[screen::CellStyle] = &[];
    let rect = Rect {
        x: 0,
        y: 0,
        w: 8,
        h: 5,
    };
    let virt = [highlight::VirtLine {
        after_line: 0,
        height: 2,
        label: "x".into(),
        path: None,
    }];
    let mut s = screen::Screen::new(8, 5);
    paint_pane(
        &mut s,
        rect,
        bytes,
        no_style,
        0,
        None,
        None,
        &[],
        &[],
        &[],
        9,
        &virt,
        &mut Vec::new(),
        false,
        &std::collections::HashMap::new(),
        &[],
        TAB_WIDTH,
    );
    assert_eq!(
        s.cell(0, 0).content,
        screen::Content::Cluster("a".into()),
        "line 0 at row 0",
    );
    assert_eq!(
        s.cell(1, 0).content,
        screen::Content::Cluster("\u{1f5bc}".into()),
        "placeholder label (🖼) on the first block row",
    );
    assert_eq!(
        s.cell(3, 0).content,
        screen::Content::Cluster("b".into()),
        "line 1 pushed below the 2-row block",
    );
}

/// F-031: on a graphics terminal, an image whose path could not be loaded (no dims) paints the
/// `[image: … (not found)]` text band, NOT blank placeholder cells, and records no graphics placement.
#[test]
fn paint_pane_shows_text_fallback_for_a_missing_image() {
    let bytes = b"a\nb";
    let no_style: &[screen::CellStyle] = &[];
    let rect = Rect {
        x: 0,
        y: 0,
        w: 40,
        h: 5,
    };
    let virt = [highlight::VirtLine {
        after_line: 0,
        height: 2,
        label: "alt".into(),
        path: Some("/no/such.png".into()),
    }];
    let mut s = screen::Screen::new(40, 5);
    let mut images = Vec::new();
    paint_pane(
        &mut s,
        rect,
        bytes,
        no_style,
        0,
        None,
        None,
        &[],
        &[],
        &[],
        9,
        &virt,
        &mut images,
        true,                              // graphics ON, but the image can't be loaded
        &std::collections::HashMap::new(), // no dims recorded → treated as not-found
        &[],
        TAB_WIDTH,
    );
    // Row 1 (just below line 0) begins the fallback band with the "[image:" text.
    assert_eq!(
        s.cell(1, 0).content,
        screen::Content::Cluster("[".into()),
        "fallback band starts with '['",
    );
    let row: String = (0..7)
        .map(|c| match &s.cell(1, c).content {
            screen::Content::Cluster(g) => g.clone(),
            _ => " ".into(),
        })
        .collect();
    assert_eq!(row, "[image:", "not-found text band, not placeholder cells");
    assert!(
        images.is_empty(),
        "an unloadable image records no graphics placement"
    );
}

/// #909: every case of Unicode's GraphemeBreakTest (17.0.0, `tests/unicode/`) paints one cell group per
/// extended grapheme cluster, in order, and the caret column of each cluster start is where the painter
/// put it: the layout never splits a cluster or drifts from the caret. Cases holding a CR are left out:
/// the painter prints a buffer CR raw and treats a CR LF cluster as no line break (#821).
#[test]
fn grapheme_break_test_clusters_paint_whole() {
    use unicode_segmentation::UnicodeSegmentation;
    let data = include_str!("../../../../../tests/unicode/GraphemeBreakTest-17.0.0.txt");
    let mut n = 0;
    for (i, raw) in data.lines().enumerate() {
        let body = raw.split('#').next().unwrap_or("").trim();
        if body.is_empty() {
            continue;
        }
        let mut text = String::new();
        let mut want: Vec<String> = Vec::new();
        for tok in body.split_whitespace() {
            match tok {
                "÷" => want.push(String::new()),
                "×" => {}
                hex => {
                    let cp = u32::from_str_radix(hex, 16).expect("hex code point");
                    let c = char::from_u32(cp).expect("scalar value");
                    text.push(c);
                    want.last_mut().expect("a case opens with ÷").push(c);
                }
            }
        }
        want.pop(); // the closing `÷` opens nothing
        if text.contains('\r') || text.contains('\n') {
            continue;
        }
        // A cluster nvim does not print shows transcribed (`^@`, `<200b>`, #1181), not whole.
        if want.iter().any(|g| screen::transcribe(g, false).is_some()) {
            continue;
        }
        let cols = 64u16;
        let rect = Rect {
            x: 0,
            y: 0,
            w: cols,
            h: 1,
        };
        let mut cur = screen::Screen::new(cols, 1);
        paint_pane(
            &mut cur,
            rect,
            text.as_bytes(),
            &[],
            0,
            None,
            None,
            &[],
            &[],
            &[],
            0,
            &[],
            &mut Vec::new(),
            false,
            &std::collections::HashMap::new(),
            &[],
            TAB_WIDTH,
        );
        let mut painted: Vec<(u16, String)> = Vec::new();
        for col in 0..cols {
            if let screen::Content::Cluster(g) = cur.cell(0, col).content {
                painted.push((col, g.to_string()));
            }
        }
        let got: Vec<&str> = painted.iter().map(|(_, g)| g.as_str()).collect();
        assert_eq!(got, want, "line {}: {text:?}", i + 1);
        for ((col, _), (b, _)) in painted.iter().zip(text.grapheme_indices(true)) {
            let (_, caret) = cursor_cell(
                text.as_bytes(),
                b,
                0,
                &[],
                &[],
                &li(text.as_bytes()),
                TAB_WIDTH,
            );
            assert_eq!(caret, *col, "caret at byte {b} of line {}: {text:?}", i + 1);
        }
        n += 1;
    }
    assert!(n > 480, "most cases are checked ({n})");
}

// ---- #615 / #616: wrap, gutter, 'list' and the horizontal offset, against nvim v0.12.6 screens ----
