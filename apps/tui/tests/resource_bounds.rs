//! Resource safety on the real key path (headless, #895): hostile counts and runaway replays finish in
//! bounded time and memory, and an interrupt stops what can run long (#682, #683, #684, #685, #787, #870,
//! #898). Expected behaviour is nvim v0.12.6's, probed headless with `-u NONE`.
//!
//! The interrupt tests install a core interrupt hook that always reports CTRL-C, so the first checkpoint a
//! long command reaches stops it: deterministic, and the same path the terminal hook drives (the PTY side
//! is `pty_interrupt.rs`).
//!
//! The bounds measure work, not wall time (#1619): a runaway count burns CPU on the test thread, so each
//! case asserts the thread's CPU time, which a loaded machine (load average ~55 under parallel builds)
//! does not inflate the way it inflates wall time. A far looser wall-clock deadline stays only as a guard
//! against a command that blocks without computing.

use std::time::{Duration, Instant};

use proptest::prelude::*;
use ruse_core::Mode;
use ruse_tui::headless::Headless;

/// The work bound, in CPU time on the test thread: generous for a debug build on a slow runner; every case
/// here is milliseconds when it is bounded (the largest, `1000000ix`, ~0.4 s), and the regressions it
/// guards are quadratic or count-proportional (40000ix once took ~30 s).
const BOUNDED: Duration = Duration::from_secs(10);

/// The hang guard, in wall time: only catches a command that stops computing yet does not return, so it
/// is loose enough that no amount of scheduling delay on a loaded machine reaches it.
const HANG_GUARD: Duration = Duration::from_secs(300);

/// CPU time consumed by the calling thread so far: unlike wall time, it does not grow while the thread
/// waits for a core.
#[cfg(unix)]
fn thread_cpu_time() -> Duration {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `ts` is a valid, writable timespec; CLOCK_THREAD_CPUTIME_ID exists on Linux and macOS.
    let rc = unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &raw mut ts) };
    assert_eq!(rc, 0, "clock_gettime(CLOCK_THREAD_CPUTIME_ID)");
    Duration::new(
        u64::try_from(ts.tv_sec).expect("non-negative seconds"),
        u32::try_from(ts.tv_nsec).expect("nanoseconds below one second"),
    )
}

/// No thread CPU clock off Unix: fall back to wall time (the bound is as load-sensitive as before there).
#[cfg(not(unix))]
fn thread_cpu_time() -> Duration {
    static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    START.get_or_init(Instant::now).elapsed()
}

/// Feed `keys` to `h` and return the CPU time it took on this thread, failing past [`HANG_GUARD`] of wall
/// time.
fn measure(h: &mut Headless, keys: &str) -> Duration {
    let (cpu0, wall0) = (thread_cpu_time(), Instant::now());
    h.keys(keys);
    let (cpu, wall) = (thread_cpu_time() - cpu0, wall0.elapsed());
    assert!(
        wall < HANG_GUARD,
        "{keys:?} did not return for {wall:?} (cpu {cpu:?})"
    );
    cpu
}

/// Feed `keys` to `h`, asserting they finish within [`BOUNDED`] of this thread's CPU time.
fn bounded(h: &mut Headless, keys: &str) {
    let cpu = measure(h, keys);
    assert!(cpu < BOUNDED, "{keys:?} took {cpu:?} of CPU");
}

fn timed(text: &str, keys: &str) -> Headless {
    let mut h = Headless::vim(text);
    bounded(&mut h, keys);
    h
}

// --- #682: search counts -------------------------------------------------------------------------------

#[test]
fn a_huge_search_count_walks_the_match_cycle_once() {
    // nvim: `99999999/foo<CR>` from line 1 lands on line 3 (the odd step of a two-match cycle).
    let h = timed("foo\nbar\nfoo\n", "99999999/foo<CR>");
    assert_eq!(h.cursor_row_col(), (3, 0));
    let h = timed("foo\nbar\nfoo\n", "/foo<CR>99999998n");
    assert_eq!(h.cursor_row_col(), (3, 0));
    let h = timed("foo\nbar\nfoo\n", "G99999999?foo<CR>");
    assert_eq!(h.cursor_row_col(), (1, 0));
    // A line offset takes the same walk.
    let h = timed("foo\nbar\nfoo\nbaz\n", "99999999/foo/+1<CR>");
    assert_eq!(h.cursor_row_col(), (4, 0));
}

#[test]
fn a_counted_search_matches_the_same_number_of_single_steps() {
    // The cycle reduction must not change where any count lands: `{n}/x` = `/x` then `n` n-1 times.
    let text = "x a x\n\nb x\nx\n c x x\n";
    for n in 1..=12 {
        let counted = timed(text, &format!("{n}/x<CR>"));
        let mut stepped = Headless::vim(text);
        stepped.keys("/x<CR>");
        for _ in 1..n {
            stepped.keys("n");
        }
        assert_eq!(counted.cursor(), stepped.cursor(), "count {n}");
        let counted = timed(text, &format!("G{n}?x<CR>"));
        let mut stepped = Headless::vim(text);
        stepped.keys("G?x<CR>");
        for _ in 1..n {
            stepped.keys("n");
        }
        assert_eq!(counted.cursor(), stepped.cursor(), "backward count {n}");
    }
}

// --- #683: count expansion -----------------------------------------------------------------------------

#[test]
fn a_huge_charwise_put_is_e1240_at_once() {
    // nvim v0.12.6: `yiw999999999p` fails with E1240 and puts nothing.
    let mut h = timed("abc\n", "yiw999999999p");
    assert_eq!(h.emsg(), "E1240");
    assert_eq!(h.text(), "abc\n");
    // Inside a macro the error stops the replay: recorded live the `x` runs, replayed it does not.
    h.keys("qqyiw999999999pxqu");
    assert_eq!(h.text(), "abc\n");
    h.keys("@q");
    assert_eq!(h.text(), "abc\n", "the x after the failed put did not run");
}

#[test]
fn a_large_count_insert_is_fast_and_one_undo_step() {
    // #1123: one edit per inserted character copied the whole buffer each time; 40000ix took ~30 s in a
    // debug build, 1000000ix now ~0.4 s. The repeats are spliced in growing chunks.
    let n = 1_000_000;
    let mut h = timed("a\n", &format!("{n}ix<Esc>"));
    assert_eq!(h.text(), format!("{}a\n", "x".repeat(n)));
    assert_eq!(h.cursor_row_col(), (1, n - 1));
    assert_eq!(h.mode(), Mode::Normal);
    h.keys("u");
    assert_eq!(h.text(), "a\n", "one undo step");
    // A multi-char body: nvim v0.12.6 `5ixy<Esc>` on `a` is `xyxyxyxyxya`, cursor col 10 (1-based); `.`
    // inserts the same five again before the cursor.
    let mut h = timed("a\n", "5ixy<Esc>");
    assert_eq!(h.text(), "xyxyxyxyxya\n");
    assert_eq!(h.cursor_row_col(), (1, 9));
    h.keys(".");
    assert_eq!(h.text(), "xyxyxyxyxxyxyxyxyxyya\n");
    assert_eq!(h.cursor_row_col(), (1, 18));
    // The batched edits replay from the trace to the same document.
    let st = h.trace().replay(b"a\n").expect("replay");
    assert_eq!(st.doc.bytes(), h.text().as_bytes());
    // An Emacs `C-u` count of a self-insert takes the same path. (The leading `a` puts the session in
    // Insert first: before any text is typed it is in Normal, where a session chord takes `C-u`.)
    let mut h = Headless::emacs("\n");
    bounded(&mut h, "a<C-u>200000x");
    assert_eq!(h.text(), format!("a{}\n", "x".repeat(200_000)));
}

#[test]
fn virtual_replace_past_the_line_end_appends_the_surplus() {
    let h = timed("abc\n", "10grx");
    assert_eq!(h.text(), "xxxxxxxxxx\n");
    assert_eq!(h.cursor_row_col(), (1, 9));
}

// --- #684: option bounds -------------------------------------------------------------------------------

#[test]
fn number_options_take_nvims_bounds() {
    let cases = [
        (":set sw=100000000000<CR>", "E474"),
        (":set sw=2147483648<CR>", "E474"),
        (":set sw=99999999999999999999<CR>", "E474"),
        (":set tw=2147483648<CR>", "E474"),
        (":set ts=10000<CR>", "E474"),
        (":set ts=0<CR>", "E487"),
    ];
    for (keys, emsg) in cases {
        let h = timed("abc\n", keys);
        assert_eq!(h.emsg(), emsg, "{keys}");
    }
    // In bounds, accepted: `>>` with sw=12 indents twelve columns (with 'expandtab').
    let h = timed("abc\n", ":set et<CR>:set sw=12<CR>>>");
    assert_eq!(h.emsg(), "");
    assert_eq!(h.text(), format!("{}abc\n", " ".repeat(12)));
}

// --- #685: undo-time and empty-register counts ---------------------------------------------------------

#[test]
fn a_huge_later_or_earlier_stops_at_the_end_of_the_undo_time_line() {
    let h = timed("abc\n", "xxx:earlier 4000000000<CR>");
    assert_eq!(h.text(), "abc\n");
    let h = timed("abc\n", "xxxuuu:later 4000000000<CR>");
    assert_eq!(h.text(), "\n");
}

#[test]
fn a_huge_count_of_an_empty_register_does_nothing() {
    // nvim: `4294967295@z` with `z` empty runs nothing and reports nothing.
    let h = timed("abc\n", "4294967295@z");
    assert_eq!(h.text(), "abc\n");
    assert_eq!(h.emsg(), "");
}

// --- #787: the recursion cap ---------------------------------------------------------------------------

#[test]
fn a_self_invoking_macro_whose_length_divides_the_cap_stops() {
    // `0@a` is 3 keys, `00@a` 4: 4 divides the 100,000-key cap, which used to reseed the budget forever.
    for body in ["0@a", "00@a", "000@a"] {
        let h = timed("abc\n", &format!("qaqqa{body}q@a"));
        assert!(h.status().contains("key limit"), "{body}: {}", h.status());
    }
}

// --- #870 ----------------------------------------------------------------------------------------------

#[test]
fn buffer_zero_is_e939_not_a_panic() {
    // nvim: `:b 0` is E939: Positive count required.
    for keys in [":b 0<CR>", ":b0<CR>", ":buffer 0<CR>"] {
        let h = timed("abc\n", keys);
        assert_eq!(h.emsg(), "E939", "{keys}");
    }
}

// --- the interrupt (#898) ------------------------------------------------------------------------------

/// A hook that reports CTRL-C at every poll.
fn always() -> bool {
    true
}

/// Run `keys` (after `setup`) with the always-CTRL-C hook installed, then remove it.
fn interrupted(text: &str, setup: &str, keys: &str) -> Headless {
    let mut h = Headless::vim(text);
    h.keys(setup);
    ruse_core::interrupt::set_hook(Some(always));
    h.keys(keys);
    ruse_core::interrupt::set_hook(None);
    assert!(
        !ruse_core::interrupt::interrupted(),
        "the session clears the interrupt once the command unwound"
    );
    h
}

#[test]
fn an_interrupt_stops_a_macro_replay() {
    let h = interrupted("abc\n", "qaqqa0@aq", "x@a");
    assert_eq!(h.status(), "Interrupted");
    // A plain command run with the hook in place is not touched (`x` ran); the replay stopped at once.
    assert_eq!(h.text(), "bc\n");
}

#[test]
fn an_interrupt_stops_a_global_before_it_runs() {
    // nvim's `ex_global`: interrupted while marking, it runs nothing.
    let h = interrupted("a\na\na\n", "", ":g/a/d<CR>");
    assert_eq!(h.text(), "a\na\na\n");
    assert_eq!(h.status(), "Interrupted");
    let h = interrupted("a\na\na\n", "", ":g/a/normal Ab<CR>");
    assert_eq!(h.text(), "a\na\na\n");
    assert_eq!(h.status(), "Interrupted");
}

#[test]
fn an_interrupt_stops_a_substitute_scan() {
    let h = interrupted("a\na\na\n", "", ":%s/a/b/<CR>");
    assert_eq!(h.text(), "a\na\na\n");
    assert_eq!(h.status(), "Interrupted");
}

#[test]
fn an_interrupt_stops_a_count_insert_and_leaves_insert() {
    let h = interrupted("\n", "", "999999999ix<Esc>");
    assert_eq!(h.text(), "x\n", "the typed x stays, no repeat ran");
    assert_eq!(h.mode(), Mode::Normal);
    assert_eq!(h.status(), "Interrupted");
}

#[test]
fn an_interrupt_stops_a_ranged_normal() {
    let h = interrupted("a\na\na\n", "", ":%normal Ab<CR>");
    assert_eq!(h.text(), "a\na\na\n");
    assert_eq!(h.status(), "Interrupted");
}

// --- #898: extreme counts never panic and always return ------------------------------------------------

/// Commands whose count is bounded by the buffer (or capped): an extreme count must return at once. The
/// count-inserts, puts and `gr` are left out: they legitimately build text proportional to the count
/// (lazily and interruptibly, tested above), which a property run has no use for.
const COMMANDS: &[&str] = &[
    "w", "b", "e", "W", "B", "E", "ge", "h", "j", "k", "l", "G", "gg", "}", "{", ")", "(", "n",
    "N", "x", "X", "dd", "yy", "J", "gJ", ">>", "<<", "~", "g~~", "gUU", "rx", "dw", "cwz<Esc>",
    "u", "<C-r>", "<C-a>", "<C-x>", "%", "|", "$", "-", "+", "_", "H", "L", "fz", "tz", ";", ",",
    "D", "C<Esc>", "s<Esc>", "S<Esc>", "dj", "yk", "g-", "g+", "@z", "]]", "[(", "])", "gM", "go",
    "<C-o>", "g;", "<C-e>", "<C-y>", "<C-f>", "<C-b>", "<C-d>", "<C-u>", "zt", "*", "#",
];

/// `n` cases, scaled by `RUSE_PROPTEST_SCALE` for the scheduled deep tier (#920), with the reject budget
/// scaled alike so a filtering test does not abort a long run.
fn cases(n: u32) -> ProptestConfig {
    let scale = std::env::var("RUSE_PROPTEST_SCALE")
        .ok()
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(1)
        .max(1);
    let base = ProptestConfig::default();
    ProptestConfig {
        cases: n.saturating_mul(scale),
        max_global_rejects: base.max_global_rejects.saturating_mul(scale),
        ..base
    }
}

proptest! {
    #![proptest_config(cases(96))]

    #[test]
    fn any_digit_run_before_any_command_returns(
        digits in prop_oneof![
            (1usize..=12).prop_map(|n| "9".repeat(n)),
            "[1-9][0-9]{0,11}",
        ],
        cmd in proptest::sample::select(COMMANDS),
        dot in any::<bool>(),
    ) {
        let text = "foo bar 12\n(baz) qux. foo\n\n  zed z\n";
        let keys = format!("/foo<CR>{digits}{cmd}{}", if dot { "." } else { "" });
        let mut h = Headless::vim(text);
        let cpu = measure(&mut h, &keys);
        prop_assert!(cpu < BOUNDED, "{} took {:?} of CPU", keys, cpu);
        prop_assert!(h.text().len() < 1 << 20, "{} grew the buffer to {}", keys, h.text().len());
    }
}
