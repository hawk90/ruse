use super::tests::ex_run;
use super::*;
use crate::input::{Ex, Feed, InputEngine};

/// `:t`/`:m` to an absolute line past the end is E16 too (#801), and the cursor rests on the last line
/// moved or copied, at the wanted column (nvim v0.12.6 `ll:2,3t0`: line 2, column 2).
#[test]
fn move_and_copy_refuse_absolute_lines_past_the_end_and_keep_the_column() {
    let mut ws = Workspace::new(b"abcd\n  defgh\nghi\n  jkl\n".to_vec());
    ws.place_focused_cursor(BytePos(2));
    ws.apply(&Command::MoveRight); // a horizontal move sets the wanted column
    ws.apply(&Command::MoveLeft);
    assert_eq!(ex_run(&mut ws, "t99").0, "E16: Invalid range");
    assert_eq!(ex_run(&mut ws, "m99").0, "E16: Invalid range");
    ex_run(&mut ws, "2,3t0");
    assert_eq!(
        ws.focused().doc.bytes(),
        b"  defgh\nghi\nabcd\n  defgh\nghi\n  jkl\n"
    );
    assert_eq!(ws.focused().view.cursor().0, 8 + 2);
    // `:d` lands on the first non-blank of the line that took the range's place.
    let mut ws = Workspace::new(b"abc\n  defgh\nghi\n  jkl\n".to_vec());
    ex_run(&mut ws, "2,3d");
    assert_eq!(ws.focused().doc.bytes(), b"abc\n  jkl\n");
    assert_eq!(ws.focused().view.cursor().0, 4 + 2);
    ex_run(&mut ws, "2d");
    assert_eq!(ws.focused().doc.bytes(), b"abc\n");
    assert_eq!(
        ws.focused().view.cursor().0,
        0,
        "through the end: the new last line"
    );
}

/// #601: `:m`/`:t` destinations relative to the cursor line, verified against nvim v0.12.6. Before the
/// fix `+1` parsed as the ABSOLUTE line 1, so `:m+1` on line 2 was a silent no-op.
#[test]
fn move_and_copy_take_relative_addresses() {
    let run = |cursor_line: usize, line: &str| {
        let mut ws = Workspace::new(b"1\n2\n3\n4\n".to_vec());
        ws.place_focused_cursor(BytePos(cursor_line * 2));
        let (status, _) = ex_run(&mut ws, line);
        (
            String::from_utf8(ws.focused().doc.bytes().to_vec()).unwrap(),
            status,
        )
    };
    assert_eq!(run(1, "m+1").0, "1\n3\n2\n4\n");
    assert_eq!(run(1, "m +1").0, "1\n3\n2\n4\n");
    assert_eq!(run(1, "m+").0, "1\n3\n2\n4\n");
    assert_eq!(run(2, "m-2").0, "1\n3\n2\n4\n");
    assert_eq!(run(1, "m.+2").0, "1\n3\n4\n2\n");
    assert_eq!(run(0, "m$-1").0, "2\n3\n1\n4\n");
    assert_eq!(run(0, "m1+2").0, "2\n3\n1\n4\n");
    assert_eq!(run(1, "t+1").0, "1\n2\n3\n2\n4\n");
    assert_eq!(run(1, "t-1").0, "1\n2\n2\n3\n4\n");
    assert_eq!(run(3, "t-3").0, "1\n4\n2\n3\n4\n");
    // Off the buffer: E16, nothing moves.
    assert_eq!(
        run(1, "m+5"),
        ("1\n2\n3\n4\n".into(), "E16: Invalid range".into())
    );
    assert_eq!(
        run(1, "t-3"),
        ("1\n2\n3\n4\n".into(), "E16: Invalid range".into())
    );
}

/// The `:w` line count matches Vim across the edge cases: empty buffer = 1 line, a trailing newline
/// does NOT add a phantom empty line, and a missing final newline still counts its line.
#[test]
fn line_count_matches_vim() {
    assert_eq!(line_count(b""), 1, "empty buffer is one line");
    assert_eq!(line_count(b"a"), 1, "single line, no newline");
    assert_eq!(line_count(b"a\n"), 1, "trailing newline is not a new line");
    assert_eq!(line_count(b"a\nb"), 2, "missing final newline still counts");
    assert_eq!(line_count(b"a\nb\n"), 2, "two lines with trailing newline");
    assert_eq!(line_count(b"\n"), 1, "a lone newline is one line");
    assert_eq!(line_count(b"\n\n"), 2, "two blank lines");
}

/// Drive the `:normal` executor directly on a known buffer and return `(bytes, cursor)`. This is the
/// hard-oracle harness the task calls for: `parity_compare` cannot drive ex commands (it feeds raw keys),
/// so every expectation below is the buffer nvim v0.12.4 produces for the SAME `:normal` line, confirmed
/// by hand (see the change evidence). `cursor` starts at `cursor0` (byte offset) before the run.
fn run_normal_oracle(
    buf: &str,
    cursor0: usize,
    range: Option<SubRange>,
    keys: &str,
) -> (String, usize) {
    let mut ws = Workspace::new(buf.as_bytes().to_vec());
    ws.place_focused_cursor(BytePos(cursor0));
    let range = match range {
        None => String::new(),
        Some(SubRange::WholeFile) => "%".into(),
        Some(SubRange::CurrentLine) => ".".into(),
        Some(SubRange::Lines(a, b)) => format!("{a},{b}"),
    };
    ex_run(&mut ws, &format!("{range}normal {keys}"));
    let bytes = String::from_utf8(ws.focused().doc.bytes().to_vec()).unwrap();
    (bytes, ws.focused().view.cursor().0)
}

#[test]
fn normal_no_range_runs_once_at_cursor() {
    // nvim: `:normal dw` with the cursor on the 3rd char of "hello world foo" → "heworld foo".
    let (bytes, _) = run_normal_oracle("hello world foo\n", 2, None, "dw");
    assert_eq!(bytes, "heworld foo\n");
    // nvim: `:normal 3x` deletes three chars from the cursor.
    let (bytes, _) = run_normal_oracle("abcdef\n", 0, None, "3x");
    assert_eq!(bytes, "def\n");
}

#[test]
fn normal_implicit_esc_terminates_insert() {
    // nvim: `:normal Ihi` (NO trailing <Esc>) inserts "hi" at the start and auto-terminates insert.
    let (bytes, _) = run_normal_oracle("abc\n", 1, None, "Ihi");
    assert_eq!(bytes, "hiabc\n");
    // An EXPLICIT Escape (the raw byte `c_CTRL-V <Esc>` types) is equivalent (the implicit one is a
    // no-op once already in Normal); `<Esc>` spelled out is five keys (#815).
    let (bytes, _) = run_normal_oracle("abc\n", 1, None, "Ihi\u{1b}");
    assert_eq!(bytes, "hiabc\n");
    let (bytes, _) = run_normal_oracle("abc\n", 1, None, "Ihi<Esc>");
    assert_eq!(bytes, "hi<Esc>abc\n");
}

#[test]
fn normal_whole_file_range_runs_per_line_at_column_zero() {
    // nvim: `:%normal A;` appends ";" to every line (A goes to each line's end).
    let (bytes, _) = run_normal_oracle("a\nbb\nccc\n", 0, Some(SubRange::WholeFile), "A;");
    assert_eq!(bytes, "a;\nbb;\nccc;\n");
    // nvim: `:2,3normal 0x` deletes the first char of lines 2 and 3 (cursor placed at column 0 each).
    let (bytes, _) = run_normal_oracle("abc\ndef\nghi\n", 0, Some(SubRange::Lines(2, 3)), "0x");
    assert_eq!(bytes, "abc\nef\nhi\n");
}

#[test]
fn normal_range_tracks_line_shifts_like_nvim() {
    // nvim: `:%normal dd` over 3 lines deletes them ALL (the fixed range end + current-line clamp).
    let (bytes, _) = run_normal_oracle("a\nb\nc\n", 0, Some(SubRange::WholeFile), "dd");
    assert_eq!(bytes, "");
    // nvim: `:1,2normal dd` on 4 lines leaves "b\nd\n" (2nd iter lands on the shifted-up 2nd line).
    let (bytes, _) = run_normal_oracle("a\nb\nc\nd\n", 0, Some(SubRange::Lines(1, 2)), "dd");
    assert_eq!(bytes, "b\nd\n");
    // nvim: `:%normal o-` appends one "-" line per ORIGINAL line — both cluster after "a" because the
    // fixed line numbers 1,2 both fall in the a-block as it grows.
    let (bytes, _) = run_normal_oracle("a\nb\n", 0, Some(SubRange::WholeFile), "o-");
    assert_eq!(bytes, "a\n-\n-\nb\n");
}

#[test]
fn normal_range_places_cursor_at_column_zero_not_first_nonblank() {
    // nvim: `:2normal x` on "   xyz" deletes the leading space at column 0 (NOT first non-blank).
    let (bytes, _) = run_normal_oracle("a\n   xyz\nb\n", 0, Some(SubRange::Lines(2, 2)), "x");
    assert_eq!(bytes, "a\n  xyz\nb\n");
}

/// Drive `:g/pat/normal {keys}` directly on a known buffer and return the resulting bytes. Like
/// [`run_normal_oracle`], every expectation is the buffer nvim v0.12.4 produces for the SAME `:g`
/// line, captured headlessly by hand (`parity_compare` cannot drive ex commands — it feeds raw keys).
fn run_global_normal_oracle(buf: &str, pattern: &str, negate: bool, keys: &str) -> String {
    let mut ws = Workspace::new(buf.as_bytes().to_vec());
    let verb = if negate { "v" } else { "g" };
    ex_run(&mut ws, &format!("{verb}/{pattern}/normal {keys}"));
    String::from_utf8(ws.focused().doc.bytes().to_vec()).unwrap()
}

#[test]
fn global_normal_appends_to_matching_lines() {
    // nvim: `:g/foo/normal A;` appends ";" to every line containing "foo".
    let out = run_global_normal_oracle("afoo\nbar\ncfoo\n", "foo", false, "A;");
    assert_eq!(out, "afoo;\nbar\ncfoo;\n");
}

#[test]
fn global_normal_v_and_bang_act_on_non_matching_lines() {
    // nvim: `:v/foo/normal A;` (and the equivalent `:g!/foo/normal A;`) append to the NON-matching line.
    let out = run_global_normal_oracle("afoo\nbar\ncfoo\n", "foo", true, "A;");
    assert_eq!(out, "afoo\nbar;\ncfoo\n");
}

#[test]
fn global_normal_deletes_first_char_of_matching_lines() {
    // nvim: `:g/^#/normal 0x` deletes the first char of every line starting with "#".
    let out = run_global_normal_oracle("#one\ntwo\n#three\n", "^#", false, "0x");
    assert_eq!(out, "one\ntwo\nthree\n");
}

#[test]
fn global_normal_stable_marks_when_line_count_grows() {
    // nvim: `:g/x/normal o-` inserts one "-" line UNDER each match — the stable two-pass shifts the
    // remaining marks past the inserted line, so it is not re-matched (each match gets exactly one dash).
    let out = run_global_normal_oracle("ax\nbx\ncy\n", "x", false, "o-");
    assert_eq!(out, "ax\n-\nbx\n-\ncy\n");
    // Every line matches → a dash under each, still no cascade.
    let out = run_global_normal_oracle("ax\nbx\n", "x", false, "o-");
    assert_eq!(out, "ax\n-\nbx\n-\n");
}

#[test]
fn global_normal_dd_equals_global_delete() {
    // nvim: `:g/foo/normal dd` deletes every matching line — identical to `:g/foo/d`.
    let out = run_global_normal_oracle("afoo\nbar\ncfoo\n", "foo", false, "dd");
    assert_eq!(out, "bar\n");
    // Interleaved matches: the mark shift keeps later deletes on their ORIGINAL text (= `:g/a/d`).
    let out = run_global_normal_oracle("a1\nb\na2\nc\n", "a", false, "dd");
    assert_eq!(out, "b\nc\n");
}

#[test]
fn global_normal_no_match_is_a_noop() {
    // No line matches → the buffer is untouched (nvim reports "Pattern not found").
    let out = run_global_normal_oracle("a\nb\nc\n", "zzz", false, "A;");
    assert_eq!(out, "a\nb\nc\n");
}

/// Drive the ex executor END TO END for `:[range]j[oin][!]`: parse the raw line with `parse_ex` and run
/// it through the session's Ex dispatch exactly as the run loop does. `parity_compare` cannot drive ex commands, so each
/// expectation is the buffer nvim v0.12.4 produces for the SAME line (`nvim -u NONE`, confirmed by hand).
fn run_ex_oracle(buf: &str, cursor0: usize, line: &str) -> (String, usize) {
    let mut ws = Workspace::new(buf.as_bytes().to_vec());
    ws.place_focused_cursor(BytePos(cursor0));
    ex_run(&mut ws, line);
    let bytes = String::from_utf8(ws.focused().doc.bytes().to_vec()).unwrap();
    (bytes, ws.focused().view.cursor().0)
}

#[test]
fn ex_join_drives_end_to_end_like_nvim() {
    // `:j` joins the current line + next on a single space; unlike `J` the cursor rests on the joined
    // line's first non-blank (nvim v0.12.6), not on the join seam.
    let (bytes, cur) = run_ex_oracle("foo\nbar\nbaz\n", 0, "j");
    assert_eq!(bytes, "foo bar\nbaz\n");
    assert_eq!(cur, 0);
    let (bytes, cur) = run_ex_oracle("x\n  foo\nbar\n", 3, "j");
    assert_eq!(bytes, "x\n  foo bar\n");
    assert_eq!(cur, 4);
    // `:j!` raw-concatenates (like `gJ`).
    let (bytes, _) = run_ex_oracle("foo\nbar\nbaz\n", 0, "j!");
    assert_eq!(bytes, "foobar\nbaz\n");
    // `:2,4j` collapses the three-line range into one.
    let (bytes, _) = run_ex_oracle("a\nb\nc\nd\ne\n", 0, "2,4j");
    assert_eq!(bytes, "a\nb c d\ne\n");
    // No space before a leading `)`.
    let (bytes, _) = run_ex_oracle("foo\n)bar\n", 0, "join");
    assert_eq!(bytes, "foo)bar\n");
    // A single-line range joins that line with the NEXT (Vim: a join needs two lines).
    let (bytes, _) = run_ex_oracle("a\nb\nc\nd\n", 0, "2j");
    assert_eq!(bytes, "a\nb c\nd\n");
}

#[test]
fn match_count_message_matches_nvim_pluralization() {
    // Verified vs nvim v0.12.4 headless (`:%s/foo//gn` and friends).
    assert_eq!(match_count_message(6, 3), "6 matches on 3 lines");
    assert_eq!(match_count_message(2, 1), "2 matches on 1 line");
    assert_eq!(match_count_message(1, 1), "1 match on 1 line");
}

/// F-009: `:s///n` drives the executor END TO END — it must echo nvim's tally, and must NOT edit the
/// buffer, move the cursor, dirty the file, or add an undo entry. The counts are what nvim v0.12.4
/// reports for the identical `:%s/foo//gn` (`nvim -u NONE`, confirmed by hand).
#[test]
fn ex_substitute_count_only_reports_without_mutating() {
    let src = "foo bar foo\nfoo baz\nno match here\nfoo foo foo\n";
    let mut ws = Workspace::new(src.as_bytes().to_vec());
    ws.place_focused_cursor(BytePos(5)); // must not move
    let s = crate::app::session::Session::ex_on(&mut ws, "%s/foo//gn");
    assert_eq!(s.status, "6 matches on 3 lines", "nvim wording");
    assert!(
        s.confirm.is_none(),
        "`n` overrides `c` — no confirm loop opens"
    );
    assert_eq!(ws.focused().doc.bytes(), src.as_bytes(), "buffer unchanged");
    assert_eq!(ws.focused().view.cursor().0, 5, "cursor did not move");
    assert!(!ws.focused().doc.is_modified(), "buffer not dirtied");
    // No undo entry was created: an undo is a genuine no-op.
    ws.apply(&Command::Undo);
    assert_eq!(
        ws.focused().doc.bytes(),
        src.as_bytes(),
        "no undo entry from :s///n"
    );

    // No-match reports E486 (nvim), still without editing.
    let (status, _) = ex_run(&mut ws, "%s/zzz//gn");
    assert_eq!(status, "E486: Pattern not found: zzz");
    assert_eq!(
        ws.focused().doc.bytes(),
        src.as_bytes(),
        "no edit on a no-match count"
    );
}

/// Whether `program` resolves on PATH — skip a shell-dependent filter test when the tool is absent.
#[cfg(unix)]
fn has_program(program: &str) -> bool {
    std::process::Command::new("sh")
        .arg("-c")
        .arg(format!("command -v {program}"))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Drive the Normal-mode `!` filter operator END TO END: feed the operator keys (`op_keys`, e.g. `!G` /
/// `!!` / `3!!`) into the input engine, take the resulting `Feed::FilterMotion`, resolve its line range
/// and seed the `:{range}!` cmdline (the SAME glue `session::run` performs), type `cmd` + `<CR>` to get
/// the finished ex line, then apply it through the SAME `:{range}!{cmd}` executor the run loop uses
/// (`range_text` → `shell::filter` → `filter_lines`). Returns `(bytes, cursor)`. Every expectation below
/// is the buffer nvim v0.12.4 produces for the identical keystrokes (verified headlessly by hand).
#[cfg(unix)]
fn run_filter_oracle(buf: &str, cursor0: usize, op_keys: &str, cmd: &str) -> (String, usize) {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ruse_core::Mode;

    let key = |c: char| KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE);
    let mut engine = InputEngine::new();
    let mut ws = Workspace::new(buf.as_bytes().to_vec());
    ws.place_focused_cursor(BytePos(cursor0));

    // 1. Operator keys → the engine hands back the linewise motion + folded count.
    let mut feed = Feed::Ignored;
    for c in op_keys.chars() {
        feed = engine.feed(key(c), Mode::Normal);
    }
    let Feed::FilterMotion { count, motion } = feed else {
        panic!("`{op_keys}` should arm the filter operator, got {feed:?}");
    };

    // 2. Frontend glue (mirrors `session::run`): number the motion's lines, open the seeded cmdline.
    let (first, last) = ws
        .reindent_range(motion, count)
        .expect("filter motion spans at least one line");
    engine.open_filter_cmdline(&format!("{},{}!", first + 1, last + 1));

    // 3. Type the shell command, then <CR> to finish the ex line.
    for c in cmd.chars() {
        engine.feed(key(c), Mode::Normal);
    }
    let submitted = engine.feed(
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        Mode::Normal,
    );
    let Feed::ExecuteEx(line) = submitted else {
        panic!("submitting the filter cmdline should yield ExecuteEx, got {submitted:?}");
    };

    // 4. Run it through the SAME executor the run loop uses for `:{range}!{cmd}`.
    let Ex::Filter { range, cmd } = crate::input::parse_ex(&line) else {
        panic!("`{line}` should parse as Ex::Filter");
    };
    let input = ws.range_text(range).expect("range has text");
    let out = crate::shell::filter(&cmd, &input).expect("shell filter runs");
    ws.filter_lines(range, out.text.as_bytes());

    let bytes = String::from_utf8(ws.focused().doc.bytes().to_vec()).unwrap();
    (bytes, ws.focused().view.cursor().0)
}

#[test]
#[cfg(unix)]
#[allow(clippy::print_stderr)] // test-only skip diagnostic when a POSIX tool is absent
fn filter_operator_bang_motion_drives_end_to_end_like_nvim() {
    if !has_program("sort") {
        eprintln!("skipping: `sort` not on PATH");
        return;
    }
    // nvim: `!Gsort<CR>` from line 1 sorts the whole buffer; cursor lands on line 1 (byte 0).
    let (bytes, cur) = run_filter_oracle("3\n1\n2\n", 0, "!G", "sort");
    assert_eq!(bytes, "1\n2\n3\n");
    assert_eq!(
        cur, 0,
        "cursor lands on the first line of the filtered region"
    );

    // nvim: `!2jsort<CR>` on 5 lines filters lines 1-3 (`!2j` = current + 2 down), leaving 4,5 alone.
    let (bytes, cur) = run_filter_oracle("5\n4\n3\n2\n1\n", 0, "!2j", "sort");
    assert_eq!(bytes, "3\n4\n5\n2\n1\n");
    assert_eq!(cur, 0);

    // nvim: `!ipsort<CR>` filters only the inner paragraph (the blank line + "keep" survive).
    let (bytes, _) = run_filter_oracle("z\ny\nx\n\nkeep\n", 0, "!ip", "sort");
    assert_eq!(bytes, "x\ny\nz\n\nkeep\n");
}

#[test]
#[cfg(unix)]
#[allow(clippy::print_stderr)] // test-only skip diagnostic when a POSIX tool is absent
fn filter_operator_double_bang_drives_end_to_end_like_nvim() {
    if !has_program("tr") || !has_program("cat") {
        eprintln!("skipping: `tr`/`cat` not on PATH");
        return;
    }
    // nvim: on line 2 (byte 4), `!!tr a-z A-Z<CR>` uppercases ONLY the current line; cursor stays line 2.
    let (bytes, cur) = run_filter_oracle("aaa\nbbb\nccc\n", 4, "!!", "tr a-z A-Z");
    assert_eq!(bytes, "aaa\nBBB\nccc\n");
    assert_eq!(cur, 4, "cursor on the first non-blank of the filtered line");

    // nvim: `3!!cat<CR>` on 4 lines is an identity filter over lines 1-3 (buffer unchanged); cursor line 1.
    let (bytes, cur) = run_filter_oracle("d\nc\nb\na\n", 0, "3!!", "cat");
    assert_eq!(bytes, "d\nc\nb\na\n");
    assert_eq!(cur, 0);

    // A count-CHANGING filter: `!!tr " " "\n"<CR>` splits "one two" into two lines (1 in, 2 out).
    let (bytes, cur) = run_filter_oracle("one two\n", 0, "!!", "tr \" \" \"\\n\"");
    assert_eq!(bytes, "one\ntwo\n");
    assert_eq!(cur, 0);
}
