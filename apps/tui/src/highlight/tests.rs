use crossterm::style::Color;
use ruse_core::Revision;

use super::*;

/// A normalized, order-independent key for comparing two span sets.
fn key(spans: &[Span]) -> Vec<(usize, usize, Color)> {
    let mut v: Vec<_> = spans.iter().map(|s| (s.start, s.end, s.style.fg)).collect();
    v.sort();
    v
}

/// The whole-document range, for tests that want unrestricted spans.
pub(super) fn all(src: &[u8]) -> std::ops::Range<usize> {
    0..src.len()
}

#[test]
fn highlights_rust() {
    let mut h = CachedHighlight::for_ext("rs").expect("rust grammar loads");
    let src = b"fn main() { let x = 1; }";
    let spans = h.spans(Revision(0), src, all(src));
    assert!(!spans.is_empty(), "produces highlight spans for real Rust");
    // `fn` (bytes 0..2) is a keyword → Magenta.
    assert!(
        spans
            .iter()
            .any(|s| s.start == 0 && s.style.fg == Color::Magenta && s.style.bold),
        "the `fn` keyword is colored",
    );
}

#[test]
fn cache_recomputes_only_on_revision_change() {
    let mut h = CachedHighlight::for_ext("rs").expect("rust grammar loads");
    let r0 = Revision(0);
    let r1 = Revision(1);

    let a = b"fn main() {}";
    let n0 = h.spans(r0, a, all(a)).len();
    assert!(n0 > 0, "first call at r0 parses");

    // Same revision AND viewport but *different* bytes: a cache hit returns the stale spans.
    let n0b = b"this is not rust at all !!!";
    let stale = h.spans(r0, n0b, all(a)).len();
    assert_eq!(
        stale, n0,
        "same key reuses the cached spans, ignoring new bytes"
    );

    // A new revision forces a recompute against the bytes actually supplied (empty → no spans).
    let n1 = h.spans(r1, b"", 0..0).len();
    assert_eq!(
        n1, 0,
        "a changed revision reparses; empty source yields no spans"
    );
}

#[test]
fn query_is_bounded_to_the_visible_range() {
    // The viewport win: a capture entirely OUTSIDE the visible range is not returned. Two functions;
    // ask only for the second line's bytes → only its `fn` keyword shows.
    let mut h = CachedHighlight::for_ext("rs").expect("rust grammar loads");
    let src = b"fn a() {}\nfn b() {}\n";
    let line2 = 10..src.len(); // from the start of `fn b`
    let spans = h.spans(Revision(0), src, line2);
    assert!(
        spans.iter().all(|s| s.end > 10),
        "no span from the hidden first line: {spans:?}"
    );
    assert!(
        spans
            .iter()
            .any(|s| s.start == 10 && s.style.fg == Color::Magenta),
        "the visible line's `fn` keyword is colored"
    );
}

#[test]
fn highlights_other_languages_by_extension() {
    // JSON and Python load + produce spans (multi-language, F-015). An unknown extension is None.
    let mut j = CachedHighlight::for_ext("json").expect("json grammar loads");
    assert!(
        !j.spans(Revision(0), br#"{"k": 1}"#, 0..8).is_empty(),
        "json produces highlight spans"
    );
    let src = b"def f():\n    return 1\n";
    let mut p = CachedHighlight::for_ext("py").expect("python grammar loads");
    assert!(
        !p.spans(Revision(0), src, all(src)).is_empty(),
        "python produces highlight spans"
    );
    assert!(
        CachedHighlight::for_ext("xyz").is_none(),
        "an unsupported extension has no highlighter"
    );
}

#[test]
fn each_supported_language_loads_and_highlights() {
    // Every dispatched extension must build a highlighter and produce at least one span for a small
    // real snippet — this pins that the per-crate query-const name (HIGHLIGHT_QUERY vs
    // HIGHLIGHTS_QUERY) and the grammar are wired correctly. Alias extensions map to the same grammar.
    let cases: &[(&str, &[u8])] = &[
        ("rs", b"fn main() { let x = 1; }"),
        ("json", br#"{"k": 1}"#),
        ("py", b"def f():\n    return 1\n"),
        ("sh", b"echo \"$HOME\"\n"),
        ("bash", b"for i in 1 2; do echo $i; done\n"),
        ("c", b"int main(void) { return 0; }"),
        ("h", b"#define N 1\n"),
        ("go", b"package main\nfunc main() {}\n"),
        ("js", b"const x = () => 42;"),
        ("jsx", b"const el = <div/>;"),
        ("css", b"a { color: red; }"),
    ];
    for (ext, src) in cases {
        let mut h = CachedHighlight::for_ext(ext).unwrap_or_else(|| panic!("{ext} grammar loads"));
        assert!(
            !h.spans(Revision(0), src, 0..src.len()).is_empty(),
            "{ext} produces highlight spans",
        );
    }
}

#[test]
fn injections_highlight_rust_macro_bodies() {
    // A macro body is a flat `token_tree` to the outer grammar: individual keyword TOKENS still
    // highlight, but STRUCTURE does not — a call `work()` is just an identifier token, never a
    // `call_expression`, so the outer query cannot color `work` as a function. The rust->rust
    // injection re-parses the body as Rust, recovering that structure. `work` at the same offset is
    // Reset without injection and @function (Blue) with it.
    let mut h = Highlight::for_ext("rs").expect("rust grammar");
    let src = b"macro_rules! m { () => { work() }; }";
    let tree = h.parser.parse(src, None).expect("parse");
    let at = src
        .windows(4)
        .position(|w| w == b"work")
        .expect("source has `work`");
    let fn_color_at = |spans: &[Span]| {
        spans
            .iter()
            .find(|s| s.start == at && s.end == at + 4)
            .map(|s| s.style.fg)
    };
    let base = h.spans_from(&tree, src, 0..src.len());
    let full = h.spans_with_injections(&tree, src, 0..src.len());
    assert_ne!(
        fn_color_at(&base),
        Some(Color::Blue),
        "outer grammar sees the call target as a flat token, not a function: {base:?}"
    );
    assert_eq!(
        fn_color_at(&full),
        Some(Color::Blue),
        "injection re-parses the macro body, coloring the call target as a function: {full:?}"
    );
}

#[test]
fn injected_spans_are_bounded_to_the_viewport() {
    // Injected highlights obey the same viewport bound as base highlights: a macro body entirely
    // above the visible range contributes nothing.
    let mut h = Highlight::for_ext("rs").expect("rust grammar");
    let src = b"macro_rules! m { () => { let x = 1; }; }\nfn tail() {}\n";
    let tree = h.parser.parse(src, None).expect("parse");
    let tail = src
        .windows(2)
        .position(|w| w == b"fn")
        .expect("has a tail fn");
    let full = h.spans_with_injections(&tree, src, tail..src.len());
    assert!(
        full.iter().all(|s| s.end > tail),
        "no injected span leaks from the hidden macro line: {full:?}"
    );
}

#[test]
fn non_injecting_grammars_are_unaffected() {
    // JSON ships no injections query, so its spans are exactly the base highlights (no panic, no
    // change). This pins that the injection path is a no-op when `injections` is None.
    let mut j = CachedHighlight::for_ext("json").expect("json grammar loads");
    assert!(
        !j.spans(Revision(0), br#"{"k": [1, 2, 3]}"#, 0..16)
            .is_empty(),
        "json still highlights with the injection path in place"
    );
}

#[test]
fn incremental_matches_full_parse_across_edits() {
    // The correctness contract for incremental parsing: reusing the previous tree must yield the
    // SAME spans as parsing each state from scratch, across a realistic edit sequence (append,
    // insert-in-middle, delete, replace). tree-sitter guarantees an incremental parse equals a
    // fresh one; this pins that our InputEdit diff feeds it correctly.
    let states: &[&[u8]] = &[
        b"fn main() {}",
        b"fn main() { let x = 1; }",
        b"fn main() {\n    let x = 1;\n}",
        b"fn main() {\n    let x = 42;\n}",
        b"fn main() {\n    let x = 42;\n    // done\n}",
        b"fn add(a: u32) -> u32 {\n    a + 1\n}",
        b"",
        b"struct S { field: String }",
    ];
    let mut inc = CachedHighlight::for_ext("rs").expect("grammar");
    for (i, s) in states.iter().enumerate() {
        let inc_spans = inc.spans(Revision(i as u64), s, all(s)).to_vec();
        let mut fresh = CachedHighlight::for_ext("rs").expect("grammar");
        let fresh_spans = fresh.spans(Revision(0), s, all(s)).to_vec();
        assert_eq!(
            key(&inc_spans),
            key(&fresh_spans),
            "incremental spans diverged from a full parse at state {i}: {:?}",
            std::str::from_utf8(s)
        );
    }
}
