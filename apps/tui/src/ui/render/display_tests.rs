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
        }
    }

    /// The painted screen with the window options `opts` set and `'tabstop'` 8 (nvim's).
    fn paint(&self, opts: impl FnMut(&mut ruse_core::WindowOptions)) -> screen::Screen {
        let mut ws = Workspace::new(self.text);
        ws.set_option_in(
            ruse_core::editor::EditorOption::TabStop(8),
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
        );
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
