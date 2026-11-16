//! The window painter against nvim screens: numbers, listchars, nowrap, the sign column. Split from `render.rs` (#1386).

use super::*;

/// The text rows a window paints, right-trimmed, for `lines` from line `top` (0-based), the cursor on
/// line `cursor`, with the window options `opts` set and `'tabstop'` 8 (nvim's).
#[allow(clippy::too_many_arguments)]
fn window_rows(
    lines: &str,
    w: u16,
    h: u16,
    top: usize,
    cursor: usize,
    leftcol: usize,
    opts: impl FnMut(&mut ruse_core::WindowOptions),
    globals: &crate::ui::window_view::DisplayGlobals,
) -> Vec<String> {
    let mut ws = Workspace::new(lines);
    ws.set_option_in(
        ruse_core::editor::EditorOption::TabStop(8),
        ruse_core::OptionLevel::Both,
    );
    ws.update_window_options(ruse_core::OptionLevel::Both, opts);
    let bytes = lines.as_bytes();
    let idx = li(bytes);
    let text = PaneText { bytes, idx: &idx };
    let mut win = WinView::resolve(
        ws.focused().view,
        globals,
        usize::from(w),
        usize::from(h),
        text.line_count(),
        false,
    );
    win.leftcol = leftcol;
    let dims = std::collections::HashMap::new();
    let p = PaneInput {
        text,
        win: &win,
        byte_style: &[],
        top,
        sel: None,
        block: None,
        hl: &[],
        underline: &[],
        conceal: &[],
        caret_line: cursor,
        cursor_line: cursor,
        virt: &[],
        folds: &[],
        signs: &[],
        visual: false,
        graphics_on: false,
        image_dims: &dims,
    };
    let mut scr = screen::Screen::new(w, h);
    let rect = Rect { x: 0, y: 0, w, h };
    paint_window(&mut scr, rect, &p, &mut Vec::new());
    (0..h)
        .map(|r| {
            (0..w)
                .filter_map(|c| match scr.cell(r, c).content {
                    screen::Content::Continuation => None,
                    screen::Content::Blank => Some(" ".to_string()),
                    screen::Content::Cluster(g) => Some(g.to_string()),
                })
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .collect()
}

/// nvim's probe buffer for the 20-column window.
const PROBE_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa bbbbbbbbbbbbbbbbbbbbbbbbbbbbbb cccccccccccccccccccccccccccccc\nshort\n\tx\u{4e2d}\u{6587}yyyyyyyyyyyyyyyyyyyy\n  indented line that is long enough to wrap around twice ok\nxxxxxxxxxxxxxxxxxx\u{4e2d}zz\ntrail   \n";

#[test]
fn number_relativenumber_showbreak_and_lastline_match_nvim() {
    let g = crate::ui::window_view::DisplayGlobals {
        showbreak: ">>".into(),
        ..Default::default()
    };
    let set = |rnu_only: bool| {
        move |o: &mut ruse_core::WindowOptions| {
            o.number = !rnu_only;
            o.relativenumber = true;
            o.linebreak = true;
            o.breakindent = true;
        }
    };
    let rows = window_rows(PROBE_A, 20, 8, 1, 2, 0, set(false), &g);
    assert_eq!(
        rows,
        [
            "  1 short",
            "3           x\u{4e2d}\u{6587}yyy",
            "    >>yyyyyyyyyyyyyy",
            "    >>yyy",
            "  1   indented line",
            "    >>that is long",
            "    >>enough to",
            "    >>wrap around@@@",
        ]
    );
    // 'relativenumber' alone: the cursor line shows 0, right-aligned.
    let rows = window_rows(PROBE_A, 20, 8, 1, 2, 0, set(true), &g);
    assert_eq!(rows[1], "  0         x\u{4e2d}\u{6587}yyy");
}

#[test]
fn listchars_match_nvim() {
    let g = crate::ui::window_view::DisplayGlobals {
        listchars: "tab:>-,trail:~,eol:$,space:.,nbsp:+".into(),
        ..Default::default()
    };
    let rows = window_rows(PROBE_A, 20, 8, 1, 2, 0, |o| o.list = true, &g);
    assert_eq!(
        rows,
        [
            "short$",
            ">-------x\u{4e2d}\u{6587}yyyyyyy",
            "yyyyyyyyyyyyy$",
            "..indented.line.that",
            ".is.long.enough.to.w",
            "rap.around.twice.ok$",
            "xxxxxxxxxxxxxxxxxx\u{4e2d}",
            "zz$",
        ]
    );
}

#[test]
fn nowrap_shows_the_columns_from_leftcol_like_nvim() {
    let g = crate::ui::window_view::DisplayGlobals::default();
    let rows = window_rows(PROBE_A, 20, 8, 0, 0, 6, |o| o.wrap = false, &g);
    assert_eq!(
        rows[..6],
        [
            "aaaaaaaaaaaaaaaaaaaa",
            "",
            "  x\u{4e2d}\u{6587}yyyyyyyyyyyyy",
            "nted line that is lo",
            "xxxxxxxxxxxx\u{4e2d}zz",
            "",
        ]
    );
    // A wide char cut at the left edge shows `<`.
    let rows = window_rows(PROBE_A, 20, 8, 0, 0, 10, |o| o.wrap = false, &g);
    assert_eq!(rows[2], "<\u{6587}yyyyyyyyyyyyyyyyy");
}

#[test]
fn sign_column_and_numberwidth_match_nvim() {
    let text = format!(
        "{}\u{4e2d}yy\nabcdefghijklmnopqrstuvwxyz\tTAB\n    indented words go on and on and on until wrap\n a  b   \nend\n",
        "x".repeat(29)
    );
    let g = crate::ui::window_view::DisplayGlobals::default();
    let yes = |o: &mut ruse_core::WindowOptions| {
        o.number = true;
        o.signcolumn = "yes".into();
    };
    let rows = window_rows(&text, 30, 10, 0, 0, 0, yes, &g);
    assert_eq!(
        rows[..9],
        [
            "    1 xxxxxxxxxxxxxxxxxxxxxxxx",
            "      xxxxx\u{4e2d}yy",
            "    2 abcdefghijklmnopqrstuvwx",
            "      yz      TAB",
            "    3     indented words go on",
            "       and on and on until wra",
            "      p",
            "    4  a  b",
            "    5 end",
        ]
    );
    let narrow = |o: &mut ruse_core::WindowOptions| {
        o.number = true;
        o.numberwidth = 1;
    };
    let rows = window_rows(&text, 30, 10, 0, 0, 0, narrow, &g);
    assert_eq!(
        rows[..4],
        [
            "1 xxxxxxxxxxxxxxxxxxxxxxxxxxxx",
            "  x\u{4e2d}yy",
            "2 abcdefghijklmnopqrstuvwxyz",
            "      TAB",
        ]
    );
}
