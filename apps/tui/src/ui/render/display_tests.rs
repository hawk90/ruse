//! The window painter against nvim v0.12.6 screens (`nvim --headless -u NONE`, `screenstring()` and
//! `screenattr()` per cell after `:redraw!`): unprintable characters (#1181), and the display options
//! #616 left open.

use super::*;
use crate::ui::window_view::DisplayGlobals;

/// A line index over `bytes`.
fn li(bytes: &[u8]) -> crate::line_index::LineIndex {
    let mut idx = crate::line_index::LineIndex::default();
    idx.refresh(ruse_core::Revision(0), bytes);
    idx
}

/// One window to paint: the buffer, its size and scroll, and what the frame overlays.
struct Win<'a> {
    text: &'a str,
    w: u16,
    h: u16,
    top: usize,
    cursor: usize,
    globals: DisplayGlobals,
    folds: Vec<crate::folds::Fold>,
    textwidth: usize,
    leftcol: usize,
    /// Rows of the top line scrolled out (`'smoothscroll'`).
    skip: usize,
}

impl<'a> Win<'a> {
    fn new(text: &'a str, w: u16, h: u16) -> Win<'a> {
        Win {
            text,
            w,
            h,
            top: 0,
            cursor: 0,
            globals: DisplayGlobals::default(),
            folds: Vec::new(),
            textwidth: 0,
            leftcol: 0,
            skip: 0,
        }
    }

    /// The painted screen with the window options `opts` set and `'tabstop'` 8 (nvim's).
    fn paint(&self, opts: impl FnMut(&mut ruse_core::WindowOptions)) -> screen::Screen {
        let mut ws = Workspace::new(self.text);
        ws.set_option_in(
            ruse_core::editor::EditorOption::TabStop(8),
            ruse_core::OptionLevel::Both,
        );
        ws.set_option_in(
            ruse_core::editor::EditorOption::TextWidth(self.textwidth),
            ruse_core::OptionLevel::Both,
        );
        ws.update_window_options(ruse_core::OptionLevel::Both, opts);
        ws.place_focused_cursor(self.cursor);
        let bytes = self.text.as_bytes();
        let idx = li(bytes);
        let text = PaneText { bytes, idx: &idx };
        let mut win = WinView::resolve(
            ws.focused().view,
            &self.globals,
            usize::from(self.w),
            usize::from(self.h),
            text.line_count(),
            false,
        )
        .with_folds(&self.folds);
        win.leftcol = self.leftcol;
        win.skip = self.skip;
        let cline = idx.line_of(self.cursor);
        win.cursor = Some((
            self.cursor,
            crate::ui::window_view::cursor_place(
                &win,
                text,
                cline,
                self.cursor - text.start(cline),
                win.tab_end(ruse_core::Mode::Normal),
            )
            .2,
        ));
        let dims = std::collections::HashMap::new();
        let p = PaneInput {
            text,
            win: &win,
            byte_style: &[],
            top: self.top,
            sel: None,
            block: None,
            hl: &[],
            underline: &[],
            conceal: &[],
            caret_line: cline,
            cursor_line: cline,
            virt: &[],
            folds: &self.folds,
            signs: &[],
            visual: false,
            graphics_on: false,
            image_dims: &dims,
        };
        let mut scr = screen::Screen::new(self.w, self.h);
        let rect = Rect {
            x: 0,
            y: 0,
            w: self.w,
            h: self.h,
        };
        paint_window(&mut scr, rect, &p, &mut Vec::new());
        scr
    }

    /// The painted rows, right-trimmed.
    fn rows(&self, opts: impl FnMut(&mut ruse_core::WindowOptions)) -> Vec<String> {
        let scr = self.paint(opts);
        (0..self.h).map(|r| row_text(&scr, r, self.w)).collect()
    }
}

fn row_text(scr: &screen::Screen, r: u16, w: u16) -> String {
    (0..w)
        .filter_map(|c| match scr.cell(r, c).content {
            screen::Content::Continuation => None,
            screen::Content::Blank => Some(" ".to_string()),
            screen::Content::Cluster(g) => Some(g.to_string()),
        })
        .collect::<String>()
        .trim_end()
        .to_string()
}

// ---- #1181: unprintable characters ----

/// nvim's probe: C0 controls, DEL, C1 controls, format characters and a control between tabs.
const CTRL: &str = "a\u{1}b\u{1b}c\u{7f}d\ne\u{85}f\u{9b}g\nh\u{a0}i\u{200b}j\u{feff}k\u{ad}l\n\u{1}\u{2}\u{3}\u{4}\u{5}\u{6}\u{7}\u{8}\u{b}\u{c}\u{d}\u{e}\u{f}\u{10}x\nm\u{2028}n\u{202e}o\u{2060}p\u{e000}q\u{10fffe}\n\t\u{1}\t|\n";

#[test]
fn control_chars_show_as_caret_and_hex_like_nvim() {
    let rows = Win::new(CTRL, 20, 7).rows(|_| {});
    assert_eq!(
        rows,
        [
            "a^Ab^[c^?d",
            "e<85>f<9b>g",
            "h\u{a0}i<200b>j<feff>k\u{ad}l",
            "^A^B^C^D^E^F^G^H^K^L",
            "^M^N^O^Px",
            "m\u{2028}n<202e>o<2060>p\u{e000}q\u{10fffe}",
            "        ^A      |",
        ]
    );
    // Without 'wrap' the same, the line cut at the edge.
    let rows = Win::new(CTRL, 20, 7).rows(|o| o.wrap = false);
    assert_eq!(rows[3], "^A^B^C^D^E^F^G^H^K^L");
    assert_eq!(rows[4], "m\u{2028}n<202e>o<2060>p\u{e000}q\u{10fffe}");
    // 'display' uhex: `<xx>` for the C0 controls too (nvim, 'nowrap').
    let mut w = Win::new(CTRL, 20, 7);
    w.globals.display = "uhex".into();
    let rows = w.rows(|o| o.wrap = false);
    assert_eq!(rows[0], "a<01>b<1b>c<7f>d");
    assert_eq!(rows[1], "e<85>f<9b>g");
    assert_eq!(rows[3], "<01><02><03><04><05>");
    assert_eq!(rows[5], "        <01>    |");
}

#[test]
fn the_cursor_sits_on_a_control_chars_first_cell_like_nvim() {
    // nvim's curswant after `l` over each line (the cursor's virtual column).
    let text = CTRL;
    let idx = li(text.as_bytes());
    let pt = PaneText {
        bytes: text.as_bytes(),
        idx: &idx,
    };
    let win = WinView::plain(20, 8);
    let vcol = |line: usize, byte: usize| {
        crate::ui::window_view::cursor_place(&win, pt, line, byte, true).2
    };
    let cases: &[(usize, &[(usize, usize)])] = &[
        (0, &[(0, 0), (1, 1), (2, 3), (3, 4), (4, 6), (5, 7), (6, 9)]),
        (1, &[(0, 0), (1, 1), (3, 5), (4, 6), (6, 10)]),
        (
            2,
            &[
                (0, 0),
                (1, 1),
                (3, 2),
                (4, 3),
                (7, 9),
                (8, 10),
                (11, 16),
                (12, 17),
            ],
        ),
        (3, &[(0, 0), (1, 2), (2, 4), (3, 6), (4, 8)]),
        (4, &[(0, 0), (1, 1), (4, 2), (5, 3), (8, 9)]),
        (5, &[(0, 7), (1, 8), (2, 15), (3, 16)]),
    ];
    for &(line, at) in cases {
        for &(byte, want) in at {
            assert_eq!(vcol(line, byte), want, "line {line} byte {byte}");
        }
    }
}

/// Strip the CSI and OSC sequences crossterm writes, leaving the printed text.
fn printed(out: &[u8]) -> Vec<u8> {
    let mut v = Vec::new();
    let mut i = 0;
    while i < out.len() {
        if out[i] == 0x1b && out.get(i + 1) == Some(&b'[') {
            i += 2;
            while i < out.len() && !(0x40..=0x7e).contains(&out[i]) {
                i += 1;
            }
            i += 1;
        } else {
            v.push(out[i]);
            i += 1;
        }
    }
    v
}

#[test]
fn no_raw_control_byte_reaches_the_terminal() {
    // The buffer, and a status-style string painted with `put_str`, both full of controls (an OSC title
    // change, a clear-screen, a bell, a C1 CSI).
    let evil = "x\u{1b}]0;pwned\u{7}y\u{1b}[2Jz\u{9b}31m\u{85}\r\u{8}\n";
    let mut scr = Win::new(evil, 40, 3).paint(|_| {});
    scr.put_str(
        2,
        0,
        "msg \u{1b}[31mred\u{7}",
        crossterm::style::Color::Reset,
        false,
    );
    let mut out = Vec::new();
    flush_diff(&mut out, &scr, &screen::Screen::new(40, 3), false).unwrap();
    let text = printed(&out);
    let s = String::from_utf8(text.clone()).unwrap();
    assert!(
        !s.chars().any(screen::is_control),
        "a control char was printed: {s:?}"
    );
    assert!(s.contains("x^[]0;pwned^Gy^[[2Jz<9b>31m<85>^M^H"), "{s:?}");
    assert!(s.contains("msg ^[[31mred^G"), "{s:?}");
}

// ---- #616: 'cursorline', 'cursorlineopt', 'cursorcolumn', 'colorcolumn' ----

/// Each cell's overlay: `c` the `CursorLine` / `CursorColumn` background, `k` the `ColorColumn` one, `B`
/// a bold `CursorLineNr` cell, `.` none (nvim's `screenattr()` classes, read from its dumps).
fn attrs(scr: &screen::Screen, w: u16, h: u16) -> Vec<String> {
    (0..h)
        .map(|r| {
            (0..w)
                .map(|c| {
                    let st = scr.cell(r, c).style;
                    if st.bg == CURSORLINE_BG {
                        'c'
                    } else if st.bg == COLORCOLUMN_BG {
                        'k'
                    } else if st.bold {
                        'B'
                    } else {
                        '.'
                    }
                })
                .collect()
        })
        .collect()
}

/// nvim's probe: a line wrapping over three 20-cell rows, a tab and a wide char, an empty line.
const CUL: &str =
    "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa bbbbbbbbbbbb\nshort\n\tx\u{4e2d}yy\n  indented\n\nend\n";

fn cul_attrs(cursor: usize, culopt: &str, number: bool) -> Vec<String> {
    let mut w = Win::new(CUL, 20, 8);
    w.cursor = cursor;
    let culopt = culopt.to_string();
    let scr = w.paint(move |o| {
        o.cursorline = true;
        o.cursorlineopt = culopt.clone();
        o.number = number;
    });
    attrs(&scr, 20, 8)
}

const ROW_C: &str = "cccccccccccccccccccc";
const ROW_N: &str = "....................";

#[test]
fn cursorline_marks_the_cursor_lines_rows_like_nvim() {
    // `both` (the default) without 'number': all three rows of the wrapped line, to the window's edge.
    let a = cul_attrs(0, "both", false);
    assert_eq!(a[..3], [ROW_C, ROW_C, ROW_C]);
    assert_eq!(a[3..], [ROW_N; 5]);
    // With 'number': the number column bold (CursorLineNr) on every row of the line.
    let a = cul_attrs(0, "both", true);
    assert_eq!(a[..3], ["BBBBcccccccccccccccc"; 3]);
    assert_eq!(a[3], ROW_N);
    // `number` alone: only the number's own row, and no line background.
    let a = cul_attrs(0, "number", true);
    assert_eq!(a[0], "BBBB................");
    assert_eq!(a[1..3], [ROW_N, ROW_N]);
    // `line` alone: the rows, not the number.
    let a = cul_attrs(0, "line", true);
    assert_eq!(a[..3], ["....cccccccccccccccc"; 3]);
    // `screenline`: the cursor's own row (after `gj`, byte 16 is on the second row of the 16-cell text).
    let a = cul_attrs(0, "screenline", true);
    assert_eq!(a[..3], ["....cccccccccccccccc", ROW_N, ROW_N]);
    let a = cul_attrs(16, "screenline", true);
    assert_eq!(a[..3], [ROW_N, "....cccccccccccccccc", ROW_N]);
    // `screenline,number`: the number's row bold, the cursor's row lit.
    let a = cul_attrs(16, "screenline,number", true);
    assert_eq!(
        a[..3],
        ["BBBB................", "....cccccccccccccccc", ROW_N]
    );
    // An empty line: the whole row.
    let empty = CUL.find("\n\nend").unwrap() + 1;
    let a = cul_attrs(empty, "both", true);
    assert_eq!(a[6], "BBBBcccccccccccccccc");
    assert_eq!(a[5], ROW_N);
}

#[test]
fn cursorline_leaves_a_visual_selection_alone() {
    let mut w = Win::new("abc\ndef\n", 10, 3);
    w.cursor = 1;
    let mut ws = Workspace::new(w.text);
    ws.update_window_options(ruse_core::OptionLevel::Both, |o| o.cursorline = true);
    let bytes = w.text.as_bytes();
    let idx = li(bytes);
    let mut win = WinView::resolve(ws.focused().view, &w.globals, 10, 3, 2, false);
    win.cursor = Some((1, 1));
    let dims = std::collections::HashMap::new();
    let p = PaneInput {
        text: PaneText { bytes, idx: &idx },
        win: &win,
        byte_style: &[],
        top: 0,
        sel: None,
        block: None,
        hl: &[],
        underline: &[],
        conceal: &[],
        caret_line: 0,
        cursor_line: 0,
        virt: &[],
        folds: &[],
        signs: &[],
        visual: true,
        graphics_on: false,
        image_dims: &dims,
    };
    let mut scr = screen::Screen::new(10, 3);
    let rect = Rect {
        x: 0,
        y: 0,
        w: 10,
        h: 3,
    };
    paint_window(&mut scr, rect, &p, &mut Vec::new());
    assert_eq!(attrs(&scr, 10, 3)[0], "..........");
}

#[test]
fn cursorcolumn_marks_the_cursors_virtual_column_like_nvim() {
    let paint = |cursor: usize| {
        let mut w = Win::new(CUL, 20, 8);
        w.cursor = cursor;
        attrs(&w.paint(|o| o.cursorcolumn = true), 20, 8)
    };
    // `3G$`: virtual column 12 on every other line's first row, past their ends too, not on the
    // wrapped rows (their columns are 20 on) and not on the cursor's own line.
    let l3_end = CUL.find("yy\n").unwrap() + 1;
    let at12 = "............c.......";
    assert_eq!(
        paint(l3_end),
        [at12, ROW_N, ROW_N, at12, ROW_N, at12, at12, at12]
    );
    // `gg0`: column 0 of the other lines.
    let at0 = "c...................";
    assert_eq!(paint(0)[..4], [ROW_N, ROW_N, ROW_N, at0]);
    assert_eq!(paint(0)[4..], [at0; 4]);
    // `gg25l`: column 25 is the long line's own second row; no other line reaches it.
    assert!(paint(25).iter().all(|r| r == ROW_N));
    // The cursor on a tab sits on its last cell (column 7).
    let l3 = CUL.find('\t').unwrap();
    assert_eq!(paint(l3)[0], ".......c............");
}

#[test]
fn colorcolumn_marks_virtual_columns_like_nvim() {
    let paint = |cc: &str, tw: usize, wrap: bool, leftcol: usize| {
        let mut w = Win::new(CUL, 20, 8);
        w.textwidth = tw;
        w.leftcol = leftcol;
        let cc = cc.to_string();
        attrs(
            &w.paint(move |o| {
                o.colorcolumn = cc.clone();
                o.wrap = wrap;
            }),
            20,
            8,
        )
    };
    // `cc=5,+2 tw=10`: columns 5 and 12 on each line's first row, past the text too.
    let cols = "....k......k........";
    assert_eq!(
        paint("5,+2", 10, true, 0),
        [cols, ROW_N, ROW_N, cols, cols, cols, cols, cols]
    );
    // `+2` needs a 'textwidth'.
    assert_eq!(paint("5,+2", 0, true, 0)[0], "....k...............");
    // `cc=25`: only the long line has a 25th column (its second row).
    let a = paint("25", 0, true, 0);
    assert_eq!(a[1], "....k...............");
    assert!(a.iter().enumerate().all(|(i, r)| i == 1 || r == ROW_N));
    // Without 'wrap', scrolled 32 columns: columns 3 and 30 are off the window.
    assert!(paint("3,30", 0, false, 32).iter().all(|r| r == ROW_N));
}

// ---- #616: 'fillchars', 'foldcolumn' ----

fn fold(start: usize, end: usize, closed: bool) -> crate::folds::Fold {
    crate::folds::Fold {
        start,
        end,
        closed,
        level: 1,
    }
}

/// nvim's fold probe: twelve lines, a fold over lines 2-8 holding one over lines 3-6 (1-based).
fn fold_text() -> String {
    (1..=12)
        .map(|i| {
            if i % 4 == 0 {
                format!("  indented {i}\n")
            } else {
                format!("line {i}\n")
            }
        })
        .collect()
}

/// The first `n` cells of each row.
fn heads(rows: &[String], n: usize) -> Vec<String> {
    rows.iter().map(|r| r.chars().take(n).collect()).collect()
}

#[test]
fn foldcolumn_shows_the_fold_levels_like_nvim() {
    let text = fold_text();
    let run = |fdc: &str, outer_closed: bool| {
        let mut w = Win::new(&text, 30, 14);
        w.folds = vec![fold(1, 7, outer_closed), fold(2, 5, true)];
        let fdc = fdc.to_string();
        w.rows(move |o| o.foldcolumn = fdc.clone())
    };
    // Both closed: the outer fold's `+`.
    assert_eq!(heads(&run("1", true), 2)[..3], [" l", "+\u{25b8}", " l"]);
    assert_eq!(heads(&run("2", true), 3)[..2], ["  l", "+ \u{25b8}"]);
    assert_eq!(heads(&run("3", true), 4)[1], "+  \u{25b8}");
    // The outer one open: `-` where it starts, `│` inside it, the inner closed one's `+`.
    let open3 = heads(&run("3", false), 3);
    assert_eq!(
        open3[..6],
        [
            "   ",
            "-  ",
            "\u{2502}+ ",
            "\u{2502}  ",
            "\u{2502}  ",
            "   "
        ]
    );
    // One column for two levels: the inner fold's `+` wins its row.
    let open1 = heads(&run("1", false), 1);
    assert_eq!(open1[..6], [" ", "-", "+", "\u{2502}", "\u{2502}", " "]);
    // `auto` is one column wide, `auto:3` as wide as the two levels.
    assert_eq!(heads(&run("auto", false), 1), open1);
    assert_eq!(
        heads(&run("auto:3", false), 2)[..4],
        ["  ", "- ", "\u{2502}+", "\u{2502} "]
    );
    // The fold column comes before the number column.
    let mut w = Win::new(&text, 30, 14);
    w.folds = vec![fold(1, 7, false), fold(2, 5, true)];
    let rows = w.rows(|o| {
        o.foldcolumn = "2".into();
        o.number = true;
    });
    assert_eq!(
        heads(&rows, 7)[..4],
        [
            "    1 l",
            "-   2 l",
            "\u{2502}+  3 \u{25b8}",
            "\u{2502}   7 l"
        ]
    );
}

#[test]
fn foldcolumn_repeats_on_a_wrapped_lines_rows() {
    let text = format!("line 1\nline 2 {}\nline 3\n", "x".repeat(30));
    let mut w = Win::new(&text, 20, 6);
    w.folds = vec![fold(1, 2, false)];
    let rows = w.rows(|o| o.foldcolumn = "2".into());
    assert_eq!(
        heads(&rows, 3),
        ["  l", "- l", "- x", "- x", "\u{2502} l", "~"]
    );
}

#[test]
fn fillchars_draws_eob_fold_and_fold_column_marks() {
    let text = fold_text();
    let mut w = Win::new(&text, 30, 14);
    w.folds = vec![fold(1, 7, true), fold(2, 5, true)];
    // The default: `~` rows after the last line, the fold row filled with `·`.
    let rows = w.rows(|_| {});
    assert_eq!(rows[6..], ["~"; 8]);
    assert!(rows[1].ends_with("\u{b7}\u{b7}\u{b7}"), "{:?}", rows[1]);
    assert_eq!(rows[1].chars().count(), 30);
    w.globals.fillchars = "eob:x,fold:-,foldopen:o,foldclose:c,foldsep:s".into();
    let rows = w.rows(|o| o.foldcolumn = "1".into());
    assert_eq!(rows[6..], ["x"; 8]);
    assert!(
        rows[1].starts_with('c') && rows[1].ends_with("---"),
        "{:?}",
        rows[1]
    );
    w.folds = vec![fold(1, 7, false), fold(2, 5, true)];
    let rows = w.rows(|o| o.foldcolumn = "2".into());
    assert_eq!(heads(&rows, 2)[..4], ["  ", "o ", "sc", "s "]);
    // A window-local value wins over the global one; `eob: ` hides the rows.
    let rows = w.rows(|o| o.fillchars = Some("eob: ".into()));
    assert_eq!(rows[13], "");
    // `lastline` marks a last line that does not fit.
    let long = format!("a\n{}\n", "b".repeat(50));
    let mut w = Win::new(&long, 20, 2);
    w.globals.fillchars = "lastline:#".into();
    assert_eq!(w.rows(|_| {})[1], "bbbbbbbbbbbbbbbbb###");
}

// ---- #615: 'smoothscroll' ----

#[test]
fn smoothscroll_marks_a_scrolled_top_line_like_nvim() {
    let text = format!("L3 {}\nL4\nL5\n", "w".repeat(45));
    // `set sms nu`, three `CTRL-E`: the top line one row in, `<<<` over the number column.
    let mut w = Win::new(&text, 20, 4);
    w.skip = 1;
    let rows = w.rows(|o| {
        o.smoothscroll = true;
        o.number = true;
    });
    assert_eq!(
        rows,
        [
            "<<< wwwwwwwwwwwwwwww",
            "    wwwwwwwwwwwwwwww",
            "  2 L4",
            "  3 L5"
        ]
    );
    // Without a gutter it covers the text's first cells.
    let rows = w.rows(|o| o.smoothscroll = true);
    assert_eq!(rows[..2], ["<<<wwwwwwwwwwwwwwwww", "wwwwwwww"]);
    // 'showbreak' leaves no marker, and neither does a window without 'smoothscroll'.
    w.globals.showbreak = ">>".into();
    assert!(w.rows(|o| o.smoothscroll = true)[0].starts_with(">>"));
    w.globals.showbreak = String::new();
    assert!(w.rows(|_| {})[0].starts_with("www"));
}

#[test]
fn fillchars_vert_and_horiz_draw_the_split_dividers() {
    let mut win = WinView::plain(10, 8);
    win.fill.vert = '*';
    win.fill.horiz = '=';
    let wins = [win.clone(), win];
    let rects = [
        Rect {
            x: 0,
            y: 0,
            w: 4,
            h: 3,
        },
        Rect {
            x: 5,
            y: 0,
            w: 4,
            h: 3,
        },
    ];
    let mut scr = screen::Screen::new(10, 4);
    draw_separators(&mut scr, &rects, SplitDir::Vertical, 10, 3, &wins);
    assert_eq!(row_text(&scr, 0, 10), "    *");
    let rects = [
        Rect {
            x: 0,
            y: 0,
            w: 10,
            h: 1,
        },
        Rect {
            x: 0,
            y: 2,
            w: 10,
            h: 1,
        },
    ];
    let mut scr = screen::Screen::new(10, 4);
    draw_separators(&mut scr, &rects, SplitDir::Horizontal, 10, 3, &wins);
    assert_eq!(row_text(&scr, 1, 10), "==========");
    // Without a window's options the defaults, as nvim's.
    let mut scr = screen::Screen::new(10, 4);
    draw_separators(&mut scr, &rects, SplitDir::Horizontal, 10, 3, &[]);
    assert_eq!(row_text(&scr, 1, 10), "\u{2500}".repeat(10));
}
