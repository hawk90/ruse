//! The line-count reports and their `'report'` threshold (#1276), against nvim v0.12.6: `nvim --clean
//! --headless`, the buffer `a1`..`a10`, `:set report=N`, the keys, then `execute('messages')`. Each row is
//! what nvim kept at `'report'` 0, 2 (the default) and 10.

use ruse_tui::headless::Headless;

const TEXT: &str = "a1\na2\na3\na4\na5\na6\na7\na8\na9\na10\n";

fn kept(report: i64, keys: &str) -> Vec<String> {
    let mut h = Headless::vim(TEXT);
    h.keys(&format!(":set report={report}<CR>"));
    h.keys(keys);
    h.messages()
}

/// What nvim keeps in `:messages`.
type Kept = &'static [&'static str];

/// `(keys, at report 0, at report 2, at report 10)`.
const CASES: &[(&str, Kept, Kept, Kept)] = &[
    // Yanks.
    ("yy", &["1 line yanked"], &[], &[]),
    ("3yy", &["3 lines yanked"], &["3 lines yanked"], &[]),
    ("yj", &["2 lines yanked"], &[], &[]),
    ("y2j", &["3 lines yanked"], &["3 lines yanked"], &[]),
    ("3y$", &["3 lines yanked"], &["3 lines yanked"], &[]),
    ("3Y", &["3 lines yanked"], &["3 lines yanked"], &[]),
    ("yw", &[], &[], &[]),
    ("vjy", &["2 lines yanked"], &[], &[]),
    ("v$y", &["2 lines yanked"], &[], &[]),
    ("Vy", &["1 line yanked"], &[], &[]),
    ("Vjjy", &["3 lines yanked"], &["3 lines yanked"], &[]),
    (
        "<C-v>jjly",
        &["block of 3 lines yanked"],
        &["block of 3 lines yanked"],
        &[],
    ),
    (
        "\"a3yy",
        &["3 lines yanked into \"a"],
        &["3 lines yanked into \"a"],
        &[],
    ),
    ("\"_3yy", &[], &[], &[]),
    (":y<CR>", &["1 line yanked"], &[], &[]),
    (":1,3y<CR>", &["3 lines yanked"], &["3 lines yanked"], &[]),
    (
        ":1,3y a<CR>",
        &["3 lines yanked into \"a"],
        &["3 lines yanked into \"a"],
        &[],
    ),
    // Deletes and changes: lines more or fewer.
    ("dd", &["1 line less"], &[], &[]),
    ("3dd", &["3 fewer lines"], &["3 fewer lines"], &[]),
    ("d2j", &["3 fewer lines"], &["3 fewer lines"], &[]),
    ("\"x3dd", &["3 fewer lines"], &["3 fewer lines"], &[]),
    ("3D", &["3 fewer lines"], &["3 fewer lines"], &[]),
    ("d/a5<CR>", &["4 fewer lines"], &["4 fewer lines"], &[]),
    ("Vjjd", &["3 fewer lines"], &["3 fewer lines"], &[]),
    ("3ccx<Esc>", &["2 fewer lines"], &[], &[]),
    ("c3jx<Esc>", &["3 fewer lines"], &["3 fewer lines"], &[]),
    ("5x", &[], &[], &[]),
    ("5J", &[], &[], &[]),
    (":d<CR>", &["1 line less"], &[], &[]),
    (":1,3d<CR>", &["3 fewer lines"], &["3 fewer lines"], &[]),
    (":1,3d x<CR>", &["3 fewer lines"], &["3 fewer lines"], &[]),
    (":1,3j<CR>", &[], &[], &[]),
    (":1,5j<CR>", &[], &[], &[]),
    // Puts and copies: lines more.
    ("yyp", &["1 line yanked", "1 more line"], &[], &[]),
    ("yyP", &["1 line yanked", "1 more line"], &[], &[]),
    (
        "yy3p",
        &["1 line yanked", "3 more lines"],
        &["3 more lines"],
        &[],
    ),
    (
        "3yyp",
        &["3 lines yanked", "3 more lines"],
        &["3 lines yanked", "3 more lines"],
        &[],
    ),
    (
        "3yygp",
        &["3 lines yanked", "3 more lines"],
        &["3 lines yanked", "3 more lines"],
        &[],
    ),
    (
        "\"a3yy\"ap",
        &["3 lines yanked into \"a", "3 more lines"],
        &["3 lines yanked into \"a", "3 more lines"],
        &[],
    ),
    (":t.<CR>", &["1 more line"], &[], &[]),
    (":1,3t$<CR>", &["3 more lines"], &["3 more lines"], &[]),
    (":1,3t0<CR>", &["3 more lines"], &["3 more lines"], &[]),
    // Moves.
    (":m+1<CR>", &["1 line moved"], &[], &[]),
    (":1,3m$<CR>", &["3 lines moved"], &["3 lines moved"], &[]),
    (":2m2<CR>", &[], &[], &[]),
    // Shifts.
    (">>", &["1 line >ed 1 time"], &[], &[]),
    ("3>>", &["3 lines >ed 1 time"], &["3 lines >ed 1 time"], &[]),
    ("<<", &["1 line <ed 1 time"], &[], &[]),
    ("3<<", &["3 lines <ed 1 time"], &["3 lines <ed 1 time"], &[]),
    ("3>j", &["4 lines >ed 1 time"], &["4 lines >ed 1 time"], &[]),
    ("V>", &["1 line >ed 1 time"], &[], &[]),
    (
        "Vjj3>",
        &["3 lines >ed 3 times"],
        &["3 lines >ed 3 times"],
        &[],
    ),
    (":><CR>", &["1 line >ed 1 time"], &[], &[]),
    (
        ":1,3><CR>",
        &["3 lines >ed 1 time"],
        &["3 lines >ed 1 time"],
        &[],
    ),
    (
        ":1,5<<CR>",
        &["5 lines <ed 1 time"],
        &["5 lines <ed 1 time"],
        &[],
    ),
    (
        ":1,3>><CR>",
        &["3 lines >ed 2 times"],
        &["3 lines >ed 2 times"],
        &[],
    ),
    // Substitutions: counted by substitutions; the count-only report always shows.
    (":s/a/b/<CR>", &["1 substitution on 1 line"], &[], &[]),
    (
        ":1,3s/a/b/<CR>",
        &["3 substitutions on 3 lines"],
        &["3 substitutions on 3 lines"],
        &[],
    ),
    (
        ":1,3s/./b/g<CR>",
        &["6 substitutions on 3 lines"],
        &["6 substitutions on 3 lines"],
        &[],
    ),
    (":s/./b/g<CR>", &["2 substitutions on 1 line"], &[], &[]),
    (
        ":%s/a/x/n<CR>",
        &["10 matches on 10 lines"],
        &["10 matches on 10 lines"],
        &["10 matches on 10 lines"],
    ),
    (
        ":s/a/x/n<CR>",
        &["1 match on 1 line"],
        &["1 match on 1 line"],
        &["1 match on 1 line"],
    ),
    // A `:g`, `:normal`, `.` and a macro.
    (
        ":g/a[1-3]$/d<CR>",
        &["3 fewer lines"],
        &["3 fewer lines"],
        &[],
    ),
    (
        ":g/a[12]$/s/./x/g<CR>",
        &["4 substitutions on 2 lines"],
        &["4 substitutions on 2 lines"],
        &[],
    ),
    (
        ":g/a[12]$/s/a/x/<CR>",
        &["2 substitutions on 2 lines"],
        &[],
        &[],
    ),
    (":norm 3dd<CR>", &["3 fewer lines"], &["3 fewer lines"], &[]),
    (
        ":1,4norm dd<CR>",
        &["1 line less", "1 line less", "1 line less", "1 line less"],
        &[],
        &[],
    ),
    (
        "4dd.",
        &["4 fewer lines", "4 fewer lines"],
        &["4 fewer lines", "4 fewer lines"],
        &[],
    ),
    (
        "qa3ddq@a",
        &["3 fewer lines", "3 fewer lines"],
        &["3 fewer lines", "3 fewer lines"],
        &[],
    ),
    // An emptied buffer says `--No lines in buffer--` (not kept) instead of the count.
    ("dG", &[], &[], &[]),
    (":%d<CR>", &[], &[], &[]),
    // E486's wording.
    (
        "/zzz<CR>",
        &["E486: Pattern not found: zzz"],
        &["E486: Pattern not found: zzz"],
        &["E486: Pattern not found: zzz"],
    ),
    (
        ":s/zzz/q/<CR>",
        &["E486: Pattern not found: zzz"],
        &["E486: Pattern not found: zzz"],
        &["E486: Pattern not found: zzz"],
    ),
];

#[test]
fn reports_match_nvim_at_report_0_2_and_10() {
    let mut wrong = Vec::new();
    for (keys, r0, r2, r10) in CASES {
        for (report, want) in [(0, r0), (2, r2), (10, r10)] {
            let got = kept(report, keys);
            if got != *want {
                wrong.push(format!(
                    "report={report} {keys}: got {got:?}, nvim {want:?}"
                ));
            }
        }
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}

#[test]
fn report_is_an_option_with_nvims_default_and_range() {
    let mut h = Headless::vim(TEXT);
    h.keys(":set report?<CR>");
    assert_eq!(h.status(), "  report=2");
    h.keys(":set report=-1<CR>");
    assert_eq!(h.status(), "E487: Argument must be positive: report=-1");
    h.keys(":set report=0<CR>:set report?<CR>");
    assert_eq!(h.status(), "  report=0");
}

#[test]
fn an_emptied_buffer_says_no_lines_in_buffer() {
    let mut h = Headless::vim(TEXT);
    h.keys("dG");
    assert_eq!(h.status(), "--No lines in buffer--");
}

/// `:sort u` reports the lines it dropped as `msgmore` does; a sort that drops none says nothing.
#[test]
fn sort_u_reports_the_lines_it_drops() {
    for (report, want) in [(0, &["2 fewer lines"][..]), (2, &[][..])] {
        let mut h = Headless::vim("x\nx\nx\na4\n");
        h.keys(&format!(":set report={report}<CR>:1,3sort u<CR>"));
        assert_eq!(h.messages(), want, "report={report}");
    }
    let mut h = Headless::vim("b\na\n");
    h.keys(":set report=0<CR>:sort<CR>");
    assert!(h.messages().is_empty());
    assert_eq!(h.text(), "a\nb\n");
}
