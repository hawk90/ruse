//! The caret column and row. Split from `render.rs` (#1386).

use super::*;

/// P6 (identity): the caret column is the documented rule — tabs to the next stop, wide clusters
/// count 2, a combining mark folds into its base's 1 — pinned by explicit expectations so the slice-0
/// refactor (paint and caret now share ONE `advance_col` rule) cannot silently drift the layout.
#[test]
fn cursor_cell_column_matches_documented_rule() {
    let cases: &[(&str, usize, u16)] = &[
        ("abc", 3, 3),        // plain ASCII
        ("a\tb", 2, 4),       // 'a' -> col 1, tab -> next stop 4
        ("\tx", 1, 4),        // leading tab -> col 4
        ("가나", 3, 2),       // one wide (width-2) cluster before the second
        ("e\u{0301}z", 3, 1), // 'e' + combining acute = one cluster, width 1
    ];
    for &(s, pos, want) in cases {
        let (_row, col) = cursor_cell(s.as_bytes(), pos, 0, &[], &[], &li(s.as_bytes()), TAB_WIDTH);
        assert_eq!(col, want, "caret column for {s:?} at byte {pos}");
    }
}

/// #601: tabs paint and place the caret at the View's own `'tabstop'`, not a fixed 4 — `:set ts=8`
/// widens a tab to the next multiple of 8 in both the caret column and the painted cells.
#[test]
fn tabs_follow_the_view_tabstop() {
    for (tab, want) in [(8u16, 8u16), (2, 2), (3, 3)] {
        let (_row, col) = cursor_cell(b"\tx", 1, 0, &[], &[], &li(b"\tx"), tab);
        assert_eq!(col, want, "caret after a leading tab at ts={tab}");
    }
    assert_eq!(line_display_col("ab\tc", 3, 8), 8);
    assert_eq!(line_byte_at_col("ab\tc", 8, 8), 3);
    // The painted row: `x` lands in the ninth cell at ts=8.
    let mut scr = screen::Screen::new(12, 1);
    let rect = Rect {
        x: 0,
        y: 0,
        w: 12,
        h: 1,
    };
    let no_dims = std::collections::HashMap::new();
    paint_pane(
        &mut scr,
        rect,
        b"\tx",
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
        &no_dims,
        &[],
        8,
    );
    assert_eq!(scr.cell(0, 8).content, screen::Content::Cluster("x".into()));
    assert_eq!(scr.cell(0, 4).content, screen::Content::Cluster(" ".into()));
}

/// #899: a line wider than `u16::MAX` cells overflowed the column arithmetic (a debug panic, a wrapped
/// caret column in release). The column now saturates at the ceiling — for plain and tab-expanded text,
/// at the caret and in the inverse map.
#[test]
fn caret_column_saturates_on_a_line_wider_than_u16() {
    let wide = "x".repeat(70_000);
    let (_row, col) = cursor_cell(
        wide.as_bytes(),
        wide.len(),
        0,
        &[],
        &[],
        &li(wide.as_bytes()),
        TAB_WIDTH,
    );
    assert_eq!(col, u16::MAX);
    let tabs = "\t".repeat(20_000); // 80000 cells
    assert_eq!(line_display_col(&tabs, tabs.len(), TAB_WIDTH), u16::MAX);
    assert_eq!(advance_col(u16::MAX, "\t", TAB_WIDTH), u16::MAX);
    assert_eq!(advance_col(u16::MAX - 1, "가", TAB_WIDTH), u16::MAX);
    // The inverse still terminates and lands inside the line.
    assert!(line_byte_at_col(&wide, u16::MAX, TAB_WIDTH) <= wide.len());
}

/// F-031 slice 3a (row model): virtual blocks above the caret's line shift the caret DOWN by their
/// height, so the drawn caret stays on its painted row.
#[test]
fn cursor_cell_row_accounts_for_virtual_blocks_above() {
    let bytes = b"a\nb\nc"; // buffer lines 0, 1, 2
    let virt = [highlight::VirtLine {
        after_line: 0,
        height: 2,
        label: "img".into(),
        path: None,
    }];
    // Caret on line 2 ('c' at byte 4): row 2 + 2 virtual rows above = screen row 4.
    assert_eq!(
        cursor_cell(bytes, 4, 0, &virt, &[], &li(bytes), TAB_WIDTH).0,
        4
    );
    // Caret on line 0 ('a' at byte 0): nothing above, row 0.
    assert_eq!(
        cursor_cell(bytes, 0, 0, &virt, &[], &li(bytes), TAB_WIDTH).0,
        0
    );
}
