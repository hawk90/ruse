//! Resource safety on the real key path (headless, #895): hostile counts and runaway replays finish in
//! bounded time and memory, and an interrupt stops what can run long (#682, #683, #684, #685, #787, #870,
//! #898). Expected behaviour is nvim v0.12.6's, probed headless with `-u NONE`.
//!
//! The interrupt tests install a core interrupt hook that always reports CTRL-C, so the first checkpoint a
//! long command reaches stops it: deterministic, and the same path the terminal hook drives (the PTY side
//! is `pty_interrupt.rs`).

use std::time::{Duration, Instant};

use proptest::prelude::*;
use ruse_core::Mode;
use ruse_tui::headless::Headless;

/// Generous for a debug build on a slow runner; every case here is milliseconds when it is bounded.
const BOUNDED: Duration = Duration::from_secs(10);

fn timed(text: &str, keys: &str) -> Headless {
    let mut h = Headless::vim(text);
    let t0 = Instant::now();
    h.keys(keys);
    assert!(t0.elapsed() < BOUNDED, "{keys:?} took {:?}", t0.elapsed());
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

proptest! {
    #![proptest_config(ProptestConfig::with_cases(96))]

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
        let t0 = Instant::now();
        let mut h = Headless::vim(text);
        h.keys(&keys);
        prop_assert!(t0.elapsed() < BOUNDED, "{} took {:?}", keys, t0.elapsed());
        prop_assert!(h.text().len() < 1 << 20, "{} grew the buffer to {}", keys, h.text().len());
    }
}
