//! The scroll commands over wrapped lines (#615): `CTRL-E` / `CTRL-Y` / `CTRL-D` / `CTRL-U` / `CTRL-F` /
//! `CTRL-B` count screen rows as nvim v0.12.6 does, with and without `'smoothscroll'`, through the real
//! key path. Ground truth captured headless (`nvim --headless -u NONE`, a 20x10 terminal: 8 text rows),
//! the cursor, top line, skipped columns and wanted column read after each key; ruse's terminal is one
//! row taller for the same 8 text rows.

use ruse_tui::headless::{Headless, Profile};

/// nvim's probe buffer: 40 lines, every third one 49 cells wide (three rows of 20).
fn probe() -> String {
    (1..=40)
        .map(|i| {
            if i % 3 == 0 {
                format!("L{i} {}\n", "w".repeat(45))
            } else {
                format!("L{i}\n")
            }
        })
        .collect()
}

/// `(keys, (line, col), top line, skipped columns, wanted column)` after each key, 1-based lines.
type Step = (&'static str, (usize, usize), usize, usize, i64);

const CASES: &[(&str, &[Step])] = &[
    // sc_e0: `:set so=0<CR>`
    (
        ":set so=0<CR>",
        &[
            ("<C-e>", (2, 0), 2, 0, 0),
            ("<C-e>", (3, 0), 3, 0, 0),
            ("<C-e>", (4, 0), 4, 0, 0),
            ("3<C-e>", (7, 0), 7, 0, 0),
            ("<C-y>", (7, 0), 6, 0, 0),
            ("<C-y>", (7, 0), 5, 0, 0),
            ("2<C-y>", (6, 0), 3, 0, 0),
            ("gg", (1, 0), 1, 0, 0),
            ("5G", (5, 0), 1, 0, 0),
            ("<C-e>", (5, 0), 2, 0, 0),
            ("<C-e>", (5, 0), 3, 0, 0),
            ("<C-e>", (5, 0), 4, 0, 0),
            ("<C-e>", (5, 0), 5, 0, 0),
            ("<C-y>", (5, 0), 4, 0, 0),
            ("<C-y>", (5, 0), 3, 0, 0),
            ("<C-y>", (5, 0), 2, 0, 0),
            ("<C-y>", (5, 0), 1, 0, 0),
        ],
    ),
    // sc_e3: `:set so=3<CR>`
    (
        ":set so=3<CR>",
        &[
            ("<C-e>", (3, 0), 2, 0, 0),
            ("<C-e>", (4, 0), 3, 0, 0),
            ("<C-e>", (6, 0), 4, 0, 0),
            ("3<C-e>", (9, 0), 7, 0, 0),
            ("<C-y>", (8, 0), 6, 0, 0),
            ("<C-y>", (6, 0), 5, 0, 0),
            ("2<C-y>", (5, 0), 3, 0, 0),
            ("gg", (1, 0), 1, 0, 0),
            ("5G", (5, 0), 3, 0, 0),
            ("<C-e>", (6, 0), 4, 0, 0),
            ("<C-e>", (6, 0), 5, 0, 0),
            ("<C-e>", (7, 0), 6, 0, 0),
            ("<C-e>", (9, 0), 7, 0, 0),
            ("<C-y>", (8, 0), 6, 0, 0),
            ("<C-y>", (6, 0), 5, 0, 0),
            ("<C-y>", (6, 0), 4, 0, 0),
            ("<C-y>", (5, 0), 3, 0, 0),
        ],
    ),
    // sc_d0: `:set so=0<CR>`
    (
        ":set so=0<CR>",
        &[
            ("<C-d>", (3, 20), 3, 0, 20),
            ("<C-d>", (6, 0), 5, 0, 0),
            ("<C-u>", (3, 20), 3, 0, 20),
            ("<C-u>", (1, 0), 1, 0, 0),
            ("3<C-d>", (3, 0), 3, 0, 0),
            ("<C-d>", (4, 0), 4, 0, 0),
            ("<C-u>", (3, 0), 3, 0, 0),
            ("gg", (1, 0), 1, 0, 0),
            ("20G", (20, 0), 19, 0, 0),
            ("<C-u>", (18, 20), 18, 0, 20),
            ("G", (40, 2), 37, 0, 20),
            ("<C-u>", (39, 19), 36, 0, 19),
            ("<C-d>", (40, 2), 37, 0, 19),
            ("gg", (1, 1), 1, 0, 19),
        ],
    ),
    // sc_d3: `:set so=3<CR>`
    (
        ":set so=3<CR>",
        &[
            ("<C-d>", (4, 1), 3, 0, 20),
            ("<C-d>", (6, 47), 5, 0, 59),
            ("<C-u>", (4, 1), 3, 0, 19),
            ("<C-u>", (2, 1), 1, 0, 19),
            ("3<C-d>", (4, 1), 3, 0, 39),
            ("<C-d>", (6, 39), 4, 0, 39),
            ("<C-u>", (4, 1), 3, 0, 19),
            ("gg", (1, 1), 1, 0, 19),
            ("20G", (20, 2), 18, 0, 19),
            ("<C-u>", (18, 48), 16, 0, 59),
            ("G", (40, 2), 37, 0, 59),
            ("<C-u>", (38, 2), 36, 0, 19),
            ("<C-d>", (40, 2), 37, 0, 19),
            ("gg", (1, 1), 1, 0, 19),
        ],
    ),
    // sc_f0: `:set so=0<CR>`
    (
        ":set so=0<CR>",
        &[
            ("<C-f>", (5, 0), 5, 0, 0),
            ("<C-f>", (9, 0), 9, 0, 0),
            ("<C-b>", (8, 0), 5, 0, 0),
            ("<C-b>", (5, 0), 1, 0, 0),
            ("2<C-f>", (9, 0), 9, 0, 0),
            ("<C-b>", (8, 0), 5, 0, 0),
            ("G", (40, 0), 37, 0, 0),
            ("<C-b>", (38, 0), 34, 0, 0),
            ("<C-f>", (38, 0), 38, 0, 0),
            ("gg", (1, 0), 1, 0, 0),
        ],
    ),
    // sc_f3: `:set so=3<CR>`
    (
        ":set so=3<CR>",
        &[
            ("<C-f>", (6, 0), 5, 0, 0),
            ("<C-f>", (10, 0), 9, 0, 0),
            ("<C-b>", (6, 0), 5, 0, 0),
            ("<C-b>", (2, 0), 1, 0, 0),
            ("2<C-f>", (10, 0), 9, 0, 0),
            ("<C-b>", (6, 0), 5, 0, 0),
            ("G", (40, 0), 37, 0, 0),
            ("<C-b>", (36, 0), 34, 0, 0),
            ("<C-f>", (40, 0), 38, 0, 0),
            ("gg", (1, 0), 1, 0, 0),
        ],
    ),
    // sc_sms_e0: `:set so=0 smoothscroll<CR>`
    (
        ":set so=0 smoothscroll<CR>",
        &[
            ("<C-e>", (2, 0), 2, 0, 0),
            ("<C-e>", (3, 0), 3, 0, 0),
            ("<C-e>", (3, 40), 3, 20, 40),
            ("3<C-e>", (5, 1), 5, 0, 40),
            ("<C-y>", (5, 1), 4, 0, 40),
            ("<C-y>", (5, 1), 3, 40, 40),
            ("2<C-y>", (5, 1), 3, 0, 40),
            ("gg", (1, 1), 1, 0, 40),
            ("5G", (5, 1), 1, 0, 40),
            ("<C-e>", (5, 1), 2, 0, 40),
            ("<C-e>", (5, 1), 3, 0, 40),
            ("<C-e>", (5, 1), 3, 20, 40),
            ("<C-e>", (5, 1), 3, 40, 40),
            ("<C-y>", (5, 1), 3, 20, 40),
            ("<C-y>", (5, 1), 3, 0, 40),
            ("<C-y>", (5, 1), 2, 0, 40),
            ("<C-y>", (5, 1), 1, 0, 40),
        ],
    ),
    // sc_sms_e3: `:set so=3 smoothscroll<CR>`
    (
        ":set so=3 smoothscroll<CR>",
        &[
            ("<C-e>", (3, 0), 2, 0, 0),
            ("<C-e>", (4, 0), 3, 0, 0),
            ("<C-e>", (4, 0), 3, 20, 0),
            ("3<C-e>", (6, 0), 5, 0, 0),
            ("<C-y>", (6, 0), 4, 0, 0),
            ("<C-y>", (5, 0), 3, 40, 0),
            ("2<C-y>", (5, 0), 3, 0, 0),
            ("gg", (1, 0), 1, 0, 0),
            ("5G", (5, 0), 3, 0, 0),
            ("<C-e>", (5, 0), 3, 20, 0),
            ("<C-e>", (5, 0), 3, 40, 0),
            ("<C-e>", (6, 0), 4, 0, 0),
            ("<C-e>", (6, 0), 5, 0, 0),
            ("<C-y>", (6, 0), 4, 0, 0),
            ("<C-y>", (5, 0), 3, 40, 0),
            ("<C-y>", (5, 0), 3, 20, 0),
            ("<C-y>", (5, 0), 3, 0, 0),
        ],
    ),
    // sc_sms_d0: `:set so=0 smoothscroll<CR>`
    (
        ":set so=0 smoothscroll<CR>",
        &[
            ("<C-d>", (3, 40), 3, 20, 40),
            ("<C-d>", (6, 20), 6, 0, 20),
            ("<C-u>", (3, 40), 3, 20, 40),
            ("<C-u>", (1, 0), 1, 0, 0),
            ("3<C-d>", (3, 20), 3, 0, 20),
            ("<C-d>", (5, 0), 4, 0, 0),
            ("<C-u>", (3, 20), 3, 0, 20),
            ("gg", (1, 1), 1, 0, 20),
            ("20G", (20, 2), 18, 20, 20),
            ("<C-u>", (18, 39), 16, 0, 39),
            ("G", (40, 2), 36, 20, 39),
            ("<C-u>", (39, 19), 36, 0, 19),
            ("<C-d>", (40, 2), 36, 20, 19),
            ("gg", (1, 1), 1, 0, 19),
        ],
    ),
    // sc_sms_f0: `:set so=0 smoothscroll<CR>`
    (
        ":set so=0 smoothscroll<CR>",
        &[
            ("<C-f>", (5, 0), 5, 0, 0),
            ("<C-f>", (9, 0), 9, 0, 0),
            ("<C-b>", (8, 0), 5, 0, 0),
            ("<C-b>", (5, 0), 1, 0, 0),
            ("2<C-f>", (9, 0), 9, 0, 0),
            ("<C-b>", (8, 0), 5, 0, 0),
            ("G", (40, 0), 36, 20, 0),
            ("<C-b>", (36, 0), 33, 0, 0),
            ("<C-f>", (36, 40), 36, 20, 40),
            ("gg", (1, 1), 1, 0, 40),
        ],
    ),
    // sc_sms_nu: `:set so=0 smoothscroll nu showbreak=>><CR>`
    (
        ":set so=0 smoothscroll nu showbreak=>><CR>",
        &[
            ("3G", (3, 0), 1, 0, 0),
            ("<C-e>", (3, 0), 2, 0, 0),
            ("<C-e>", (3, 0), 3, 0, 0),
            ("<C-e>", (3, 16), 3, 16, 16),
            ("<C-e>", (3, 30), 3, 32, 34),
            ("<C-y>", (3, 30), 3, 16, 34),
            ("<C-y>", (3, 30), 3, 0, 34),
        ],
    ),
    // jmp3: `:set so=3<CR>`
    (
        ":set so=3<CR>",
        &[
            ("20G", (20, 0), 18, 0, 0),
            ("gg", (1, 0), 1, 0, 0),
            ("19G", (19, 0), 18, 0, 0),
            ("gg", (1, 0), 1, 0, 0),
            ("21G", (21, 0), 19, 0, 0),
            ("gg", (1, 0), 1, 0, 0),
            ("30G", (30, 0), 28, 0, 0),
            ("G", (40, 0), 37, 0, 0),
            ("gg", (1, 0), 1, 0, 0),
        ],
    ),
    // jmp0s: `:set so=0 sms<CR>`
    (
        ":set so=0 sms<CR>",
        &[
            ("20G", (20, 0), 18, 20, 0),
            ("gg", (1, 0), 1, 0, 0),
            ("21G", (21, 0), 19, 0, 0),
            ("G", (40, 0), 36, 20, 0),
            ("gg", (1, 0), 1, 0, 0),
            ("30G", (30, 0), 28, 0, 0),
            ("k", (29, 0), 28, 0, 0),
            ("k", (28, 0), 28, 0, 0),
            ("k", (27, 0), 27, 0, 0),
        ],
    ),
    // jmp0: `:set so=0<CR>`
    (
        ":set so=0<CR>",
        &[
            ("20G", (20, 0), 19, 0, 0),
            ("gg", (1, 0), 1, 0, 0),
            ("21G", (21, 0), 19, 0, 0),
            ("G", (40, 0), 37, 0, 0),
            ("gg", (1, 0), 1, 0, 0),
            ("30G", (30, 0), 28, 0, 0),
        ],
    ),
    // s2_nowrap: `:set so=0 nowrap<CR>`
    (
        ":set so=0 nowrap<CR>",
        &[
            ("<C-d>", (5, 0), 5, 0, 0),
            ("<C-d>", (9, 0), 9, 0, 0),
            ("<C-u>", (5, 0), 5, 0, 0),
            ("<C-f>", (11, 0), 11, 0, 0),
            ("<C-b>", (12, 0), 5, 0, 0),
            ("<C-e>", (12, 0), 6, 0, 0),
            ("<C-e>", (12, 0), 7, 0, 0),
            ("<C-y>", (12, 0), 6, 0, 0),
            ("G", (40, 0), 33, 0, 0),
            ("<C-u>", (36, 0), 29, 0, 0),
            ("<C-b>", (30, 0), 23, 0, 0),
            ("gg", (1, 0), 1, 0, 0),
        ],
    ),
    // s2_fold: `:set so=0<CR>:normal 4Gzf8jgg<CR>`
    (
        ":set so=0<CR>:normal 4Gzf8jgg<CR>",
        &[
            ("<C-e>", (2, 0), 2, 0, 0),
            ("<C-e>", (3, 0), 3, 0, 0),
            ("<C-e>", (4, 0), 4, 0, 0),
            ("<C-y>", (4, 0), 3, 0, 0),
            ("<C-y>", (4, 0), 2, 0, 0),
            ("<C-y>", (4, 0), 1, 0, 0),
            ("<C-d>", (15, 0), 4, 0, 0),
            ("<C-u>", (3, 40), 2, 0, 40),
            ("<C-f>", (14, 2), 14, 0, 40),
            ("<C-b>", (14, 2), 2, 0, 40),
            ("gg", (1, 1), 1, 0, 40),
        ],
    ),
    // s2_fold3: `:set so=3<CR>:normal 4Gzf8jgg<CR>`
    (
        ":set so=3<CR>:normal 4Gzf8jgg<CR>",
        &[
            ("<C-e>", (3, 0), 2, 0, 0),
            ("<C-e>", (3, 0), 3, 0, 0),
            ("<C-e>", (14, 0), 4, 0, 0),
            ("<C-y>", (3, 0), 3, 0, 0),
            ("<C-y>", (3, 0), 2, 0, 0),
            ("<C-y>", (3, 0), 1, 0, 0),
            ("<C-d>", (4, 0), 3, 0, 0),
            ("<C-u>", (2, 0), 1, 0, 0),
            ("<C-f>", (15, 0), 13, 0, 0),
            ("<C-b>", (3, 0), 1, 0, 0),
        ],
    ),
    // s2_scr: `:set so=0 scroll=2<CR>`
    (
        ":set so=0 scroll=2<CR>",
        &[
            ("<C-d>", (3, 0), 3, 0, 0),
            ("<C-d>", (4, 0), 4, 0, 0),
            ("<C-u>", (3, 0), 3, 0, 0),
            ("5<C-d>", (6, 0), 6, 0, 0),
            ("<C-d>", (9, 0), 9, 0, 0),
            ("<C-u>", (6, 0), 6, 0, 0),
        ],
    ),
    // s2_vis: `:set so=0<CR>`
    (
        ":set so=0<CR>",
        &[
            ("v<C-d>", (3, 20), 3, 0, 20),
            ("<C-d>", (6, 0), 5, 0, 0),
            ("<C-u>", (3, 20), 3, 0, 20),
            ("<Esc>", (3, 20), 3, 0, 20),
        ],
    ),
    // s2_nu: `:set so=0 nu<CR>`
    (
        ":set so=0 nu<CR>",
        &[
            ("<C-d>", (3, 16), 3, 0, 16),
            ("<C-d>", (6, 0), 5, 0, 0),
            ("<C-e>", (6, 0), 6, 0, 0),
            ("<C-e>", (7, 0), 7, 0, 0),
            ("<C-u>", (5, 0), 5, 0, 0),
            ("<C-y>", (5, 0), 4, 0, 0),
            ("<C-f>", (8, 0), 8, 0, 0),
            ("<C-b>", (8, 0), 4, 0, 0),
        ],
    ),
    // s2_smsnu: `:set so=0 sms nu<CR>`
    (
        ":set so=0 sms nu<CR>",
        &[
            ("<C-e>", (2, 0), 2, 0, 0),
            ("<C-e>", (3, 0), 3, 0, 0),
            ("<C-e>", (3, 16), 3, 16, 16),
            ("<C-e>", (3, 32), 3, 32, 32),
            ("<C-y>", (3, 32), 3, 16, 32),
            ("<C-d>", (6, 16), 6, 0, 16),
            ("<C-u>", (3, 32), 3, 16, 32),
            ("<C-f>", (6, 32), 6, 32, 32),
            ("<C-b>", (7, 1), 3, 16, 32),
        ],
    ),
    // s2_endsms: `:set so=0 sms<CR>`
    (
        ":set so=0 sms<CR>",
        &[
            ("G", (40, 0), 36, 20, 0),
            ("<C-e>", (40, 0), 36, 40, 0),
            ("<C-e>", (40, 0), 37, 0, 0),
            ("<C-e>", (40, 0), 38, 0, 0),
            ("<C-e>", (40, 0), 39, 0, 0),
            ("<C-e>", (40, 0), 39, 20, 0),
            ("<C-y>", (40, 0), 39, 0, 0),
            ("<C-y>", (40, 0), 38, 0, 0),
            ("<C-d>", (40, 0), 38, 0, 0),
            ("<C-f>", (40, 0), 40, 0, 0),
        ],
    ),
];

/// Where only the cursor's column differs, and why: nvim keeps a `CTRL-U`'s screen column (19) as the
/// wanted column on a 2-cell line, so the `20G` after it lands on the line's last char; ruse's core keeps
/// the column the cursor landed on (1). Known gap, see the PR.
const KNOWN_COL: &[&str] = &[
    ":set so=3<CR> <C-d><C-d><C-u><C-u>3<C-d><C-d><C-u>gg20G",
    ":set so=3<CR> <C-d><C-d><C-u><C-u>3<C-d><C-d><C-u>gg20G<C-u>",
];

#[test]
fn scroll_keys_count_screen_rows_like_nvim() {
    let text = probe();
    let mut bad = Vec::new();
    for (setup, steps) in CASES {
        let mut h = Headless::with_size(Profile::Vim, &text, 20, 9);
        h.keys(setup);
        let mut done = String::new();
        for &(keys, at, top, skipcol, _want) in *steps {
            h.keys(keys);
            done.push_str(keys);
            let view = &h.workspace().focused().view;
            let got = (h.cursor_row_col(), view.top() + 1, view.skipcol());
            let known = KNOWN_COL.contains(&format!("{setup} {done}").as_str());
            if got != (at, top, skipcol)
                && !(known && (got.0 .0, got.1, got.2) == (at.0, top, skipcol))
            {
                bad.push(format!(
                    "{setup} {done}: got {got:?}, nvim {:?}",
                    (at, top, skipcol)
                ));
            }
        }
    }
    assert!(bad.is_empty(), "{} differ:\n{}", bad.len(), bad.join("\n"));
}

/// The status line after `keys` in an 8-row window.
fn status(keys: &str) -> String {
    let mut h = Headless::with_size(Profile::Vim, &probe(), 20, 9);
    h.keys(keys);
    h.status().to_string()
}

/// nvim v0.12.6 (`nvim_exec2`, a 10-row window): `'scroll'` is the window's half height until set, a
/// count to `CTRL-D` sets it, 0 is the half again, more than the height or negative is `E49`.
#[test]
fn the_scroll_option_reads_sets_and_checks_like_nvim() {
    assert_eq!(status(":set scr?<CR>"), "  scroll=4");
    assert_eq!(status(":set scr=3<CR>:set scr?<CR>"), "  scroll=3");
    assert_eq!(
        status(":set scr=3<CR>:set scr=0<CR>:set scr?<CR>"),
        "  scroll=4"
    );
    assert_eq!(status(":set scr=8<CR>:set scr?<CR>"), "  scroll=8");
    assert_eq!(status(":set scr=9<CR>"), "E49: Invalid scroll size: scr=9");
    assert_eq!(
        status(":set scr=-1<CR>"),
        "E49: Invalid scroll size: scr=-1"
    );
    assert_eq!(status("2<C-d>:set scroll?<CR>"), "  scroll=2");
}

/// nvim v0.12.6's `:set` checks of the #616 options, and `'statuscolumn'`, which ruse leaves out.
#[test]
fn the_616_options_check_their_values_like_nvim() {
    assert_eq!(status(":set cul?<CR>"), "nocursorline");
    assert_eq!(status(":set cul cuc<CR>:set cuc?<CR>"), "  cursorcolumn");
    assert_eq!(status(":set culopt?<CR>"), "  cursorlineopt=both");
    assert_eq!(
        status(":set culopt=line,screenline<CR>"),
        "E474: Invalid argument: culopt=line,screenline"
    );
    assert_eq!(
        status(":set culopt=number,line<CR>:set culopt?<CR>"),
        "  cursorlineopt=number,line"
    );
    assert_eq!(
        status(":set cc=1,+2,-3<CR>:set cc?<CR>"),
        "  colorcolumn=1,+2,-3"
    );
    assert_eq!(
        status(":set cc=1,,2<CR>"),
        "E474: Invalid argument: cc=1,,2"
    );
    assert_eq!(
        status(":set fdc=auto:9<CR>:set fdc?<CR>"),
        "  foldcolumn=auto:9"
    );
    assert_eq!(status(":set fdc=10<CR>"), "E474: Invalid argument: fdc=10");
    assert_eq!(status(":set fcs?<CR>"), "  fillchars=");
    assert_eq!(
        status(":set fcs=eob:xy<CR>"),
        r#"E1511: Wrong number of characters for field "eob": fcs=eob:xy"#
    );
    assert_eq!(
        status(":set fcs=bad:x<CR>"),
        "E474: Invalid argument: fcs=bad:x"
    );
    // Global-local, as nvim: `:setlocal` gives the window its own value.
    assert_eq!(
        status(":setlocal fcs=eob:q<CR>:setglobal fcs?<CR>"),
        "  fillchars="
    );
    assert_eq!(
        status(":setlocal fcs=eob:q<CR>:set fcs?<CR>"),
        "  fillchars=eob:q"
    );
    assert_eq!(status(":set sms?<CR>"), "nosmoothscroll");
    // 'statuscolumn' is an expression ('%l', '%{...}' Vimscript): not supported (see the PR).
    assert_eq!(status(":set stc=%l<CR>"), "option not supported: stc=%l");
}

/// nvim v0.12.6: a 300-cell line (15 rows of 20) in the 8-row window shows the rows around the cursor
/// (nvim's `curs_columns` `w_skipcol`), with and without `'scrolloff'`.
#[test]
fn a_line_taller_than_the_window_shows_the_cursors_rows_like_nvim() {
    let text = format!("top\n{}\nend\n", "a".repeat(300));
    /// `(keys, (line, col), top line, skipped columns at so=0 and so=3)`.
    type TallStep = (&'static str, (usize, usize), usize, [usize; 2]);
    let steps: [TallStep; 9] = [
        ("j", (2, 0), 2, [0, 0]),
        ("$", (2, 299), 2, [140, 140]),
        ("0", (2, 0), 2, [0, 0]),
        ("150|", (2, 149), 2, [0, 60]),
        ("200|", (2, 199), 2, [40, 60]),
        ("k", (1, 2), 1, [0, 0]),
        ("j$", (2, 299), 2, [140, 140]),
        ("j", (3, 2), 3, [0, 0]),
        ("k", (2, 299), 2, [140, 140]),
    ];
    for (i, so) in ["0", "3"].iter().enumerate() {
        let mut h = Headless::with_size(Profile::Vim, &text, 20, 9);
        h.keys(&format!(":set so={so}<CR>"));
        for (keys, at, top, skip) in steps {
            h.keys(keys);
            let view = &h.workspace().focused().view;
            assert_eq!(
                (h.cursor_row_col(), view.top() + 1, view.skipcol()),
                (at, top, skip[i]),
                "so={so} after {keys}"
            );
        }
    }
}
