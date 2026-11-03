//! Screen-line motions, horizontal scroll and the wrapped viewport (#615) through the real key path,
//! against nvim v0.12.6 ground truth captured headless (`nvim --clean`, `:set so=0`, the window sizes
//! given). A ruse terminal one row taller than nvim's `lines - 2` gives the same text rows.

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
    let mut h = Headless::with_size(Profile::Vim, text, cols, 9);
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
    let mut h = Headless::with_size(Profile::Vim, &text, 40, 24);
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
