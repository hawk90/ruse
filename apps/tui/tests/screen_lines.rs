//! Screen-line motions, horizontal scroll and the wrapped viewport (#615) through the real key path,
//! against nvim v0.12.6 ground truth captured headless (`nvim --clean`, `:set so=0`, the window sizes
//! given). A ruse terminal of nvim's `lines` gives the same `lines - 2` text rows (the bar and one
//! `'cmdheight'` row).

use ruse_tui::headless::{Headless, Profile};

/// nvim's probe buffer: a 92-cell line, a short one, tab + wide chars, an indented long line.
fn probe_a() -> String {
    let first = format!("{} {} {}", "a".repeat(30), "b".repeat(30), "c".repeat(30));
    format!(
        "{first}\nshort\n\tx\u{4e2d}\u{6587}{}\n  indented line that is long enough to wrap around twice ok\n{}\u{4e2d}zz\ntrail   \n",
        "y".repeat(20),
        "x".repeat(18),
    )
}

/// A session `cols` wide with 8 text rows, `'scrolloff'` 0 and `'tabstop'` 8 as in `nvim --clean`.
fn session(text: &str, cols: u16) -> Headless {
    let mut h = Headless::with_size(Profile::Vim, text, cols, 10);
    h.keys(":set so=0 ts=8<CR>");
    h
}

fn at(h: &Headless) -> (usize, usize) {
    h.cursor_row_col()
}

#[test]
fn gj_gk_move_over_wrapped_rows() {
    let mut h = session(&probe_a(), 20);
    for want in [(1, 20), (1, 40), (1, 60)] {
        h.keys("gj");
        assert_eq!(at(&h), want);
    }
    h.keys("gk");
    assert_eq!(at(&h), (1, 40));
    h.keys("gg0gj3lg$");
    assert_eq!(at(&h), (1, 39), "g$ ends the screen row");
    h.keys("g0");
    assert_eq!(at(&h), (1, 20), "g0 starts it");
    h.keys("gg0gm");
    assert_eq!(at(&h), (1, 10), "gm is half the window across");
    h.keys("gjgm");
    assert_eq!(at(&h), (1, 30));
    h.keys("4G0gjg^");
    assert_eq!(at(&h), (4, 21), "g^ skips the row's leading blank");
}

/// A 30-column window: a wide char that does not fit in the last cell goes to the next row (a `>` in
/// its place), and a tab can straddle two rows.
fn probe_d() -> String {
    format!(
        "{}\u{4e2d}yy\nabcdefghijklmnopqrstuvwxyz\tTAB\n    indented words go on and on and on until wrap\n a  b   \nend\n",
        "x".repeat(29)
    )
}

#[test]
fn gj_lands_on_split_wide_chars_and_tabs() {
    let mut h = session(&probe_d(), 30);
    h.keys("gg0gj");
    assert_eq!(at(&h), (1, 29), "the wide char moved to row 2");
    h.keys("gk");
    assert_eq!(at(&h), (1, 0));
    h.keys("gg028lgj");
    assert_eq!(at(&h), (1, 33), "past the row's text: the line's last char");
    h.keys("2G0gj");
    assert_eq!(at(&h), (2, 26), "the tab straddling the rows");
    h.keys("2G04lgj");
    assert_eq!(at(&h), (2, 29));
    h.keys("3G02g$");
    assert_eq!(at(&h), (3, 48), "2g$ ends the second row");
    h.keys("3G03gj");
    assert_eq!(at(&h), (5, 0), "a count crosses lines");
    h.keys("5Ggj");
    assert_eq!(at(&h), (5, 0), "gj on the last row fails");
    h.keys("gggk");
    assert_eq!(at(&h), (1, 0), "gk on the first row fails");
}

#[test]
fn gj_counts_showbreak_and_breakindent() {
    let mut h = session(&probe_d(), 30);
    h.keys(":set bri briopt=min:10 sbr=+<CR>");
    h.keys("3G04lgj");
    assert_eq!(at(&h), (3, 30));
    h.keys("gj");
    assert_eq!(at(&h), (4, 4));
    h.keys("g0");
    assert_eq!(at(&h), (4, 0));
    h.keys("g0gm");
    assert_eq!(at(&h), (4, 7), "gm past the line's end: its last char");
}

/// nvim's operator probe (80 columns, every line one row): `gj` / `gk` / `gm` / `g0` are exclusive and
/// charwise, `g$` inclusive, `dgj` from a line's start is linewise.
#[test]
fn screen_motions_under_operators() {
    let text = format!(
        "one\ntwo\nthree\nfour\n{}\nsix\nseven\neight\n",
        "L".repeat(35)
    );
    let run = |keys: &str| {
        let mut h = Headless::with_size(Profile::Vim, &text, 80, 24);
        h.keys(keys);
        h.text()
    };
    let lines = |v: &[&str]| v.iter().map(|l| format!("{l}\n")).collect::<String>();
    let ls = "L".repeat(35);
    assert_eq!(
        run("gg0dgj"),
        lines(&["two", "three", "four", &ls, "six", "seven", "eight"])
    );
    assert_eq!(
        run("5G03ldgj"),
        lines(&["one", "two", "three", "four", "LLLx", "seven", "eight"])
    );
    assert_eq!(
        run("5G013ldgk"),
        lines(&[
            "one",
            "two",
            "three",
            &format!("fou{}", "L".repeat(22)),
            "six",
            "seven",
            "eight"
        ])
    );
    assert_eq!(
        run("5G03ldg$"),
        lines(&["one", "two", "three", "four", "LLL", "six", "seven", "eight"])
    );
    assert_eq!(
        run("5G03lvgjd"),
        lines(&["one", "two", "three", "four", "LLLseven", "eight"])
    );
    assert_eq!(
        run("5G0dgm"),
        lines(&["one", "two", "three", "four", "L", "six", "seven", "eight"])
    );
    assert_eq!(
        run("5G013ldg0"),
        lines(&[
            "one",
            "two",
            "three",
            "four",
            &"L".repeat(22),
            "six",
            "seven",
            "eight"
        ])
    );
}

fn leftcol(h: &Headless) -> usize {
    h.workspace().focused().view.leftcol()
}

#[test]
fn nowrap_scrolls_sideways_like_nvim() {
    let mut h = session(&probe_a(), 20);
    h.keys(":set nowrap<CR>gg025l");
    assert_eq!((at(&h), leftcol(&h)), ((1, 25), 6), "'sidescroll' 1");
    h.keys("zh");
    assert_eq!(
        (at(&h), leftcol(&h)),
        ((1, 24), 5),
        "the cursor stays in the window"
    );
    h.keys("zl");
    assert_eq!((at(&h), leftcol(&h)), ((1, 24), 6));
    h.keys("3zl");
    assert_eq!((at(&h), leftcol(&h)), ((1, 24), 9));
    h.keys("zH");
    assert_eq!((at(&h), leftcol(&h)), ((1, 19), 0), "half a window");
    h.keys("zL");
    assert_eq!((at(&h), leftcol(&h)), ((1, 19), 10));
    h.keys("gg040|zs");
    assert_eq!((at(&h), leftcol(&h)), ((1, 39), 39));
    h.keys("ze");
    assert_eq!((at(&h), leftcol(&h)), ((1, 39), 20));
    h.keys("$");
    assert_eq!(
        (at(&h), leftcol(&h)),
        ((1, 91), 81),
        "far off: the cursor in the middle"
    );
    h.keys("0");
    assert_eq!((at(&h), leftcol(&h)), ((1, 0), 0));
    h.keys(":set siso=3<CR>017l");
    assert_eq!((at(&h), leftcol(&h)), ((1, 17), 1), "'sidescrolloff' 3");
    h.keys("0gm");
    assert_eq!(at(&h), (1, 10));
    h.keys("g$");
    assert_eq!((at(&h), leftcol(&h)), ((1, 19), 3));
    h.keys("g0");
    assert_eq!((at(&h), leftcol(&h)), ((1, 3), 0));
}

#[test]
fn horizontal_scroll_does_nothing_while_wrapping() {
    let mut h = session(&probe_a(), 20);
    h.keys("gg010lzlzs");
    assert_eq!((at(&h), leftcol(&h)), ((1, 10), 0));
}

/// nvim: 40 lines, line 10 wrapped over 4 rows of a 20-column, 8-row window.
#[test]
fn the_viewport_counts_wrapped_rows() {
    let text: String = (1..=40)
        .map(|i| {
            if i == 10 {
                format!("L10 {}\n", "w".repeat(70))
            } else {
                format!("line {i}\n")
            }
        })
        .collect();
    let top = |h: &Headless| h.workspace().focused().view.top() + 1;
    let mut h = session(&text, 20);
    for (keys, want) in [
        ("5G", 1),
        ("7G", 1),
        ("8G", 1),
        ("9G", 2),
        ("10G", 6),
        ("11G", 7),
        ("12G", 8),
        ("kkkk", 8),
        ("kk", 6),
    ] {
        h.keys(keys);
        assert_eq!(top(&h), want, "so=0 after {keys}");
    }
    h.keys(":set so=3<CR>gg");
    for (keys, want) in [
        ("6G", 2),
        ("8G", 6),
        ("10G", 8),
        ("13G", 11),
        ("14G", 11),
        ("k", 11),
        ("k", 10),
        ("k", 10),
        ("k", 8),
        ("k", 7),
        ("k", 6),
        ("G", 33),
    ] {
        h.keys(keys);
        assert_eq!(top(&h), want, "so=3 after {keys}");
    }
}

/// nvim: a jump further than about half the window puts the cursor line in the middle (100 lines, 23
/// rows, `scrolloff=3`).
#[test]
fn a_far_jump_centres_the_cursor_line() {
    let text: String = (1..=100).map(|i| format!("l{i}\n")).collect();
    let mut h = Headless::with_size(Profile::Vim, &text, 40, 25);
    h.keys(":set so=3<CR>");
    let top = |h: &Headless| h.workspace().focused().view.top() + 1;
    for (keys, want) in [
        ("50G", 39),
        ("20G", 9),
        ("45G", 34),
        ("46G", 34),
        ("75G", 64),
        ("60G", 57),
        ("30G", 19),
        ("29G", 19),
    ] {
        h.keys(keys);
        assert_eq!(top(&h), want, "after {keys}");
    }
}

/// #1539: `.` after an operator over a screen-line motion resolves the motion again from the new
/// cursor, as nvim's redo buffer holds the keys and not the span (nvim v0.12.6, 80 columns, 120-char
/// lines that wrap over two rows).
#[test]
fn dot_repeats_the_screen_motion_not_the_span() {
    let l = "0123456789".repeat(12);
    let run = |lines: usize, keys: &str| {
        let text: String = (0..lines).map(|_| format!("{l}\n")).collect();
        let mut h = Headless::with_size(Profile::Vim, &text, 80, 24);
        h.keys(keys);
        let text = h.text();
        (text, h.register('"'))
    };
    let lines = |v: &[&str]| v.iter().map(|l| format!("{l}\n")).collect::<String>();
    let forty = "0123456789".repeat(4);
    // `dgj` deletes 80 chars; on the 40 left, `gj` reaches the next line's start, so `.` is linewise.
    assert_eq!(
        run(3, "dgj."),
        (lines(&[&l, &l]), (format!("{forty}\n"), "linewise"))
    );
    // `dg$` then `.` deletes the rest of the row the cursor is on now.
    assert_eq!(
        run(2, "dg$."),
        (lines(&["", &l]), (forty.clone(), "charwise"))
    );
    // `cgj` replays its text over the new span: the line `X…` changed linewise.
    assert_eq!(run(3, "cgjX<Esc>.").0, lines(&["X", &l, &l]));
    // `3dgk` joins two lines into one of three rows; `.` goes up three rows of that one again.
    assert_eq!(
        run(4, "G40l3dgk.").0,
        lines(&[&format!("{}{}", &l[..39], &l[40..])])
    );
}

/// #1576: after `$`, `dgk` is inclusive and goes back, so it also deletes the char under the cursor
/// (nvim v0.12.6, 80 columns): 41 chars from column 79 through the line's last char, leaving 79.
/// Without `$` (`G90ldgk`) and with `dvgk` it stays exclusive.
#[test]
fn backward_dgk_after_dollar_takes_the_cursor_char() {
    let l = "0123456789".repeat(12);
    let run = |keys: &str| {
        let text: String = (0..4).map(|_| format!("{l}\n")).collect();
        let mut h = Headless::with_size(Profile::Vim, &text, 80, 24);
        h.keys(keys);
        let t = h.text();
        let last = t.lines().last().unwrap_or_default().len();
        (last, h.register('"').0.len(), h.cursor_row_col())
    };
    assert_eq!(run("G$dgk"), (79, 41, (4, 78)));
    assert_eq!(run("G$ygk"), (120, 41, (4, 79)));
    assert_eq!(run("G$dvgk"), (80, 40, (4, 79)));
    assert_eq!(run("G90ldgk"), (40, 80, (4, 10)));
}

/// #1577: `.` after an operator over `H` / `M` / `L` resolves the viewport line again, as nvim's redo
/// buffer holds the keys and not the lines (nvim v0.12.6, 23 text rows, 'scrolloff' 3).
#[test]
fn dot_repeats_h_m_l_not_the_lines() {
    let text: String = (1..=100).map(|i| format!("line {i}\n")).collect();
    let run = |keys: &str| {
        let mut h = Headless::with_size(Profile::Vim, &text, 80, 25);
        h.keys(":set so=3<CR>");
        h.keys(keys);
        let t = h.text();
        let first = t.lines().next().unwrap_or_default().to_string();
        (t.lines().count(), first, h.cursor_row_col().0)
    };
    // `dL` deletes the 23 lines shown; `.` deletes the 23 shown after it.
    assert_eq!(run("dL."), (54, "line 47".to_string(), 1));
    assert_eq!(run("dM."), (76, "line 25".to_string(), 1));
    assert_eq!(run("cLx<Esc>.").0, 56);
    // `.` after scrolling resolves `L` in the scrolled window.
    assert_eq!(run("dL5<C-e>."), (57, "line 24".to_string(), 9));
}
