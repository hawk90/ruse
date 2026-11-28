use super::*;

fn env(cino: &str) -> CEnv<'static> {
    let cino: &'static str = Box::leak(cino.to_string().into_boxed_str());
    CEnv {
        ts: crate::cells::Tabs::uniform(8),
        cino: CinOptions::parse(cino, 4),
        cinwords: "if,else,while,do,for,switch",
        cinscopedecls: DEFAULT_CINSCOPEDECLS,
        cinkeys: DEFAULT_CINKEYS,
        comments: "s1:/*,mb:*,ex:*/,://,b:#,:%,:XCOMM,n:>,fb:-",
    }
}

fn ind(src: &str, row: usize, cino: &str) -> Option<usize> {
    let lines: Vec<&[u8]> = src.split('\n').map(str::as_bytes).collect();
    line_indent(&lines, row, 0, false, &env(cino))
}

#[test]
fn basic_c_layout() {
    assert_eq!(ind("int f() {\nx;", 1, ""), Some(4));
    assert_eq!(ind("int f() {\n    if (a)\nx;", 2, ""), Some(8));
    assert_eq!(ind("int f() {\n    if (a)\n        x;\ny;", 3, ""), Some(4));
    assert_eq!(ind("int f() {\n    x = a\n? b", 2, ""), Some(8));
    assert_eq!(ind("int f() {\n    g(a,\nb", 2, ""), Some(12));
    assert_eq!(ind("int f() {\n    x;\n}", 2, ""), Some(0));
    assert_eq!(ind("int f() {\n    switch (a) {\ncase 1:", 2, ""), Some(8));
    assert_eq!(
        ind("int f() {\n    switch (a) {\ncase 1:", 2, ":0"),
        Some(4)
    );
    assert_eq!(ind("int f() {\nout:", 1, ""), Some(0));
    assert_eq!(ind("int f() {\n#if A", 1, ""), Some(0));
    assert_eq!(ind("int f() {\n    /* a\nb", 2, ""), Some(7));
    assert_eq!(ind("int f() {\n    /*\n*", 2, ""), Some(5));
    // A top-level `{` takes cino-f (0): the object `{` JavaScript indents is not cindent's.
    assert_eq!(ind("var o =\n{", 1, ""), Some(0));
}

/// A C file of `n`-odd lines of the usual shape (functions, blocks, comments, a switch,
/// continuation lines), unindented.
fn big_c(n: usize) -> String {
    let unit = "/* f does x\n * and y */\nint f(int a,\nint b)\n{\nint x = a +\nb;\nif (a) {\n\
x++; // inc\n} else {\nx--;\n}\nswitch (x) {\ncase 1:\nbreak;\ndefault:\nx = g(a,\nb);\n}\n\
while (x)\nx--;\nreturn x;\n}\n\n";
    let per = unit.matches('\n').count();
    unit.repeat(n / per + 1)
}

/// `=G` over a 10k-line C file (#1163):
/// `cargo test -p ruse-core cindent_eq_cost -- --ignored --nocapture`.
#[test]
#[ignore = "timing probe; quarantined in tests/quarantine.yaml (#910)"]
#[allow(clippy::print_stdout)] // the timings are the output
fn cindent_eq_cost() {
    let src = big_c(10_000);
    let lines = src.matches('\n').count();
    let e = env("");
    let t = std::time::Instant::now();
    let cols = reindent(src.as_bytes(), 0, lines - 1, &e);
    println!("=G over {lines} lines: {:?}", t.elapsed());
    // One key or open at the end (the per-key path splits the buffer and marks its lines).
    let ls: Vec<&[u8]> = src.split('\n').map(str::as_bytes).collect();
    let t = std::time::Instant::now();
    let _ = line_indent(&ls, lines - 3, 0, true, &e);
    println!("one line at the end: {:?}", t.elapsed());
    assert_eq!(cols[5], Some(4)); // `int x = a +`
    assert_eq!(cols[6], Some(8)); // its continuation
}
