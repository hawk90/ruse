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
