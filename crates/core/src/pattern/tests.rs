use super::*;

fn find(pat: &str, hay: &str) -> Option<(usize, usize)> {
    let re = Regex::compile(pat, Options::default()).expect("compiles");
    re.find_at(hay, BytePos(0)).map(|m| (m.start.0, m.end.0))
}

/// #745: every `\<word\>` match `*` steps through and `hlsearch` paints follows Vim's class edges,
/// including the ones a failed candidate skips over; `:s///g` replaces the same ones.
#[test]
fn edge_word_boundaries_scan_every_match() {
    let spans = |pat: &str, hay: &str, sub: bool| -> Vec<(usize, usize)> {
        let re = Regex::compile(pat, Options::default()).expect("compiles");
        let all = if sub {
            re.find_all(hay)
        } else {
            re.search_all(hay)
        };
        all.iter().map(|m| (m.start.0, m.end.0)).collect()
    };
    assert_eq!(
        spans(r"\<API\>", "x API를 API\nxAPI API", false),
        [(2, 5), (9, 12), (18, 21)]
    );
    assert_eq!(spans(r"\<한", "abc한글 한", false), [(3, 6), (10, 13)]);
    assert_eq!(spans(r"\<", "ab한 c", false), [(0, 0), (2, 2), (6, 6)]);
    assert_eq!(spans(r"\>", "ab한 c", true), [(2, 2), (5, 5), (7, 7)]);
    assert_eq!(spans(r"\<\(a\|ab\)\>", "ab a", true), [(0, 2), (3, 4)]);
    assert_eq!(find(r"\<API\zs를", "API를"), Some((3, 6)));
}

/// The edge check sees only the pattern's top-level first and last pieces of a single branch.
#[test]
fn only_edge_word_boundaries_are_dropped() {
    let low = |p: &str| lower(p, Magic::Magic, false).expect("lowers");
    let l = low(r"\<foo\>");
    assert!(l.bow && l.eow && l.asserts == 0 && !l.variable);
    assert_eq!(l.rust, "foo");
    let l = low(r"\<a\|b\>");
    assert!(!l.bow && !l.eow && l.asserts == 2);
    let l = low(r"\(\<a\)");
    assert!(!l.bow && l.asserts == 1);
    let l = low(r"\<\<a");
    assert!(l.bow && l.asserts == 1);
    let l = low(r"^\<a.*\>");
    assert!(!l.bow && l.eow && l.asserts == 2 && l.variable);
}

/// Only a pattern with a line-break item is multi-line (Vim's `RF_HASNL`).
#[test]
fn multiline_marks_line_break_items() {
    let ml = |p: &str| Regex::compile(p, Options::default()).unwrap().multiline();
    assert!(ml(r"a\nb") && ml(r"\_s") && ml(r"\_[ab]") && ml(r"[a\n]"));
    assert!(!ml("ab") && !ml(r"a\s") && !ml("[ab]") && !ml(r"\_^a"));
}

#[test]
fn literal_and_dot_star() {
    assert_eq!(find("foo", "a foo b"), Some((2, 5)));
    assert_eq!(find("f.o", "a fxo b"), Some((2, 5)));
    assert_eq!(find("a.*b", "xaZZbx"), Some((1, 5)));
}

/// Default magic (`\m`): `(` `+` are LITERAL; `\(` `\+` are the operators — the inversion the
/// front-end owns.
#[test]
fn default_magic_literalness() {
    // bare `(` is a literal paren
    assert_eq!(find("f(x", "a f(x b"), Some((2, 5)));
    // bare `+` is a literal plus
    assert_eq!(find("a+", "xa+y"), Some((1, 3)));
    // `\+` is one-or-more
    assert_eq!(find("a\\+", "xaaay"), Some((1, 4)));
    // `\(ab\)\+` groups and repeats
    assert_eq!(find("\\(ab\\)\\+", "zababz"), Some((1, 5)));
}

/// Very-magic (`\v`): `(` `+` are operators bare, like Rust/PCRE.
#[test]
fn very_magic() {
    assert_eq!(find("\\v(ab)+", "zababz"), Some((1, 5)));
    assert_eq!(find("\\va+", "xaaay"), Some((1, 4)));
}

/// Very-nomagic (`\V`): everything literal — `.` matches a real dot only.
#[test]
fn very_nomagic_all_literal() {
    assert_eq!(find("\\Va.b", "xaxbx"), None); // `.` is literal here
    assert_eq!(find("\\Va.b", "xa.bx"), Some((1, 4)));
}

/// `\zs` resets the reported match START; `\ze` the END. The context still has to match, but the
/// reported span is only the middle — the atom with no PCRE equivalent (design §4).
#[test]
fn no_match_in_the_slot_after_the_final_newline() {
    let re = Regex::compile("^", Options::default()).unwrap();
    let hay = "abc\ndef\n";
    assert_eq!(re.find_at(hay, BytePos(1)).map(|m| m.start.0), Some(4));
    assert_eq!(re.find_at(hay, BytePos(5)), None);
    let starts: Vec<usize> = re.find_all(hay).iter().map(|m| m.start.0).collect();
    assert_eq!(starts, vec![0, 4]);
    let re = Regex::compile("^$", Options::default()).unwrap();
    assert_eq!(re.find_at(hay, BytePos(0)), None);
    assert_eq!(re.find_at("abc\n\ndef\n", BytePos(5)), None);
    // A haystack without a final newline (one line, an empty buffer) keeps its end match.
    assert_eq!(
        re.find_at("", BytePos(0)),
        Some(Match {
            start: BytePos(0),
            end: BytePos(0)
        })
    );
    assert_eq!(find("$", "abc"), Some((3, 3)));
}

#[test]
fn zs_ze_boundary_override() {
    // `foo\zsbar` reports only `bar`, but only when preceded by `foo`.
    assert_eq!(find("foo\\zsbar", "a foobar b"), Some((5, 8)));
    assert_eq!(find("foo\\zsbar", "a xxxbar b"), None);
    // `\zsbar\ze` — both boundaries.
    assert_eq!(find("o\\zsba\\zer", "a foobar b"), Some((5, 7)));
    // `\ze` only: `foo\ze` reports the empty span before... reports up to \ze = the `foo` start..\ze.
    assert_eq!(find("foo\\zebar", "a foobar b"), Some((2, 5)));
}

#[test]
fn smartcase_upper_forces_sensitive() {
    let ci = Options {
        ignore_case: true,
        smart_case: true,
        ..Options::default()
    };
    // all-lowercase pattern → case-insensitive
    assert!(Regex::compile("foo", ci)
        .unwrap()
        .find_at("a FOO b", BytePos(0))
        .is_some());
    // an uppercase in the pattern → case-sensitive, so `FOO` won't match `foo`
    assert!(Regex::compile("Foo", ci)
        .unwrap()
        .find_at("a foo b", BytePos(0))
        .is_none());
}

#[test]
fn find_all_non_overlapping() {
    let re = Regex::compile("a\\+", Options::default()).unwrap();
    let ms: Vec<_> = re
        .find_all("aa b aaa")
        .iter()
        .map(|m| (m.start.0, m.end.0))
        .collect();
    assert_eq!(ms, vec![(0, 2), (5, 8)]);
}

fn all(pat: &str, hay: &str) -> Vec<(usize, usize)> {
    Regex::compile(pat, Options::default())
        .expect("compiles")
        .find_all(hay)
        .iter()
        .map(|m| (m.start.0, m.end.0))
        .collect()
}

/// Regression for the `find_all` infinite loop on a zero-width match at end-of-string (a search/
/// `hlsearch`/`:s///g` DoS reachable from `/a*`, `/$`, `/.*`, or empty `/`). Every one of these
/// patterns matches empty at `hay.len()`; the old hand-rolled scan could not advance past it and
/// spun forever. Each `all(...)` call below would hang, not fail, if the bug returned — so this test
/// IS the termination gate. The expected spans are Vim-faithful, captured from nvim v0.12.4 via
/// `:s/<pat>/-/g` (a dash marks each match; a trailing dash means a match at end-of-line):
///   "abc" + `x*`  -> "-a-b-c"      (empty before each char, NONE at EOL)
///   "aaa" + `a*`  -> "-"            (one whole-line match, no trailing empty)
///   "aax" + `a*`  -> "-x"           (only "aa"; no empty adjacent to it, none at EOL)
///   "abc" + `$`   -> "abc-"         (a lone anchored empty match AT EOL is kept)
///   "abc" + `.*`  -> "-"            (greedy whole-line match, no trailing empty)
///   ""    + `x*`  -> "-"            (the SOLE empty match on an empty line is kept)
///   "foo bar" + `o*` -> "-f- -b-a-r"
#[test]
fn find_all_zero_width_at_eol_terminates_and_matches_nvim() {
    assert_eq!(all("x*", "abc"), vec![(0, 0), (1, 1), (2, 2)]);
    assert_eq!(all("a*", "aaa"), vec![(0, 3)]);
    assert_eq!(all("a*", "aax"), vec![(0, 2)]);
    assert_eq!(all("$", "abc"), vec![(3, 3)]);
    assert_eq!(all(".*", "abc"), vec![(0, 3)]);
    assert_eq!(all("x*", ""), vec![(0, 0)]);
    assert_eq!(
        all("o*", "foo bar"),
        vec![(0, 0), (1, 3), (4, 4), (5, 5), (6, 6)]
    );
}

/// `:s///g` resumes at the reported (`\ze`) end, and keeps an empty match at the end of a line that a
/// match (not a one-character step) reached (#1047, #1048; nvim v0.12.6: `:s/\v(ab)\zeab/-/g` on
/// "ababab" is "--ab", `:s/\>/-/g` on "a b" is "a- b-", `:s/a\|$/-/g` on "ab" is "-b-"). Each line of a
/// multi-line haystack is scanned on its own.
#[test]
fn find_all_follows_do_sub() {
    assert_eq!(all(r"\v(ab)\zeab", "ababab"), vec![(0, 2), (2, 4)]);
    assert_eq!(all(r"\>", "a b"), vec![(1, 1), (3, 3)]);
    assert_eq!(all(r"\>", "foo bar"), vec![(3, 3), (7, 7)]);
    assert_eq!(all(r"a\|$", "ab"), vec![(0, 1), (2, 2)]);
    assert_eq!(all("x*", "ab\ncd"), vec![(0, 0), (1, 1), (3, 3), (4, 4)]);
    assert_eq!(all("^", "ab\ncd\n"), vec![(0, 0), (3, 3)]);
}

/// A `/` search sees every empty match (`\<` at each word start, `$` at each line end) and resumes at
/// the reported end (#705).
#[test]
fn search_all_keeps_empty_matches() {
    let spans = |pat: &str, hay: &str| -> Vec<(usize, usize)> {
        Regex::compile(pat, Options::default())
            .expect("compiles")
            .search_all(hay)
            .iter()
            .map(|m| (m.start.0, m.end.0))
            .collect()
    };
    assert_eq!(spans(r"\<", "xx foo bar"), vec![(0, 0), (3, 3), (7, 7)]);
    assert_eq!(spans("$", "ab\ncd\n"), vec![(2, 2), (5, 5)]);
    assert_eq!(spans("^$", "a\n\nb"), vec![(2, 2)]);
    assert_eq!(spans("x*", "ab"), vec![(0, 0), (1, 1), (2, 2)]);
    assert_eq!(spans(r"foo\zsbar", "foobar foobar"), vec![(3, 6), (10, 13)]);
    assert_eq!(spans("aa", "aaa"), vec![(0, 2)]);
    assert_eq!(spans(r"a\nb", "a\nb\na\nb"), vec![(0, 3), (4, 7)]);
}

/// Vim allows nine `\(` groups (E51 beyond); `\%(` and the `\zs` markers do not count.
#[test]
fn at_most_nine_capture_groups() {
    let nine = r"\(a\)".repeat(9);
    assert!(Regex::compile(&nine, Options::default()).is_ok());
    assert!(Regex::compile(&format!(r"{nine}\%(b\)\zs"), Options::default()).is_ok());
    assert!(matches!(
        Regex::compile(&format!(r"{nine}\(b\)"), Options::default()),
        Err(RegexError::Syntax(_))
    ));
}

/// A belt-and-suspenders wall-clock guard: even if the expectations above were ever loosened, a
/// regressed `find_all` must never hang. Run the worst zero-width-at-EOL case in a worker thread and
/// require it to finish well under a generous budget.
#[test]
fn find_all_zero_width_never_hangs() {
    use std::sync::mpsc;
    use std::time::Duration;
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let n = Regex::compile("a*", Options::default())
            .unwrap()
            .find_all("aaaa")
            .len();
        let _ = tx.send(n);
    });
    assert!(
        rx.recv_timeout(Duration::from_secs(5)).is_ok(),
        "find_all did not terminate (zero-width-at-EOL infinite loop regressed)"
    );
}

#[test]
fn non_greedy_star() {
    // `\{-}` is Vim's non-greedy `*` — `a.\{-}b` matches the SHORTEST `a…b`.
    let re = Regex::compile("a.\\{-}b", Options::default()).unwrap();
    assert_eq!(
        re.find_at("axbxb", BytePos(0)),
        Some(Match {
            start: BytePos(0),
            end: BytePos(3)
        })
    );
}

/// Rejects, never fakes: unrepresentable atoms return a typed error (design §2).
#[test]
fn unsupported_atoms_are_rejected() {
    assert!(matches!(
        Regex::compile("\\(a\\)\\1", Options::default()),
        Err(RegexError::Unsupported(_))
    ));
    assert!(matches!(
        Regex::compile("foo\\@=", Options::default()),
        Err(RegexError::Unsupported(_))
    ));
    // A position atom needs buffer context the matcher does not have. (`\%(ab\)` used to be pinned
    // here as rejected; it is a plain non-capturing group, which nvim matches and ruse now lowers.)
    assert!(matches!(
        Regex::compile("\\%Vab", Options::default()),
        Err(RegexError::Unsupported(_))
    ));
    assert_eq!(find("\\%(ab\\)\\+", "xabab"), Some((1, 5)));
}
