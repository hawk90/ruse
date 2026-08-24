//! Full-stack keystroke FUZZER (testing-and-benchmarks.md §1.2 property + §1.9 e2e): the layer the
//! micro suites miss. The oracle corpus checks short single ops; the core property tests fuzz `Command`
//! streams; the input-engine property test fuzzes random (key, mode) pairs. NONE of them drive the
//! COMPOSED stack — keystroke -> real input engine (mode following the REAL editor state, as
//! `main.rs::run` does) -> Command -> edit -> undo — over a long realistic session, which is exactly
//! where a modal editor's state-interaction bugs live (mode transitions × operators × counts × registers
//! × dot-repeat × undo grouping). A failure here is a real defect; proptest shrinks to a minimal repro.
//!
//! Two invariants, split by soundness (the same split the core `undo_all_then_redo_all_round_trips`
//! makes by generating edits only):
//!   * ANY session (incl. `u`/CTRL-R) — the buffer stays valid UTF-8 with the cursor on a char boundary
//!     after every key, and a full undo terminates and returns to the initial buffer. `undo-all -> root`
//!     holds regardless of how the session branched the undo tree.
//!   * A MONOTONIC session (no in-session undo/redo, so history only grows) — the full undo/redo round
//!     trip holds: undo-all -> initial, then redo-all -> edited. This is ill-posed once a session undoes
//!     mid-way (it can end at an interior node with an orphaned redo), which is why it is a separate test
//!     over a restricted alphabet rather than an assertion on the general one.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use proptest::prelude::*;
use ruse_core::{apply_command, Command, EditorState};
use ruse_tui::input::{Feed, InputEngine};

/// A realistic Vim keystroke alphabet: motions, operators, mode switches, edits, counts, find/search,
/// registers, dot-repeat, and the multi-key control prefixes. Uniform selection over this set builds
/// long sessions that exercise the grammar's compositions (`d3w`, `"ayyp`, `cwX<Esc>.`, `/x<CR>n`, …).
/// `include_history_ops` adds `u` (undo) and CTRL-R (redo) — keys that MUTATE history rather than only
/// extend it, so they are excluded from the round-trip test's monotonic alphabet.
fn alphabet(include_history_ops: bool) -> Vec<KeyEvent> {
    let mut v = Vec::new();
    // Motions h j k l w b e W B E 0 $ ^ g G; modes i a o O v V R; edits x X p P J ~ D C Y s; operators
    // d c y; find f F t T ; ,; search / n N; register "; dot .; counts 1 2 3; and literal insert chars
    // (so insert sessions produce real bytes, incl. a multibyte one).
    for c in "hjklwbeWBE0$^gGiaoOvVRxXpPJ~DCYsdcyfFtT;,nN/\".123AIqZ é".chars() {
        v.push(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
    }
    for code in [KeyCode::Esc, KeyCode::Enter, KeyCode::Backspace] {
        v.push(KeyEvent::new(code, KeyModifiers::NONE));
    }
    // CTRL-O insert one-shot, CTRL-V blockwise, CTRL-^ Lang-Arg toggle — history-neutral prefixes.
    for c in ['o', 'v', '^'] {
        v.push(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL));
    }
    if include_history_ops {
        v.push(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::NONE)); // undo
        v.push(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL)); // redo
    }
    v
}

/// A small random UTF-8 starting buffer (incl. multibyte chars — the cursor-boundary stressor).
fn text() -> impl Strategy<Value = Vec<u8>> {
    prop::collection::vec(any::<char>(), 0..24)
        .prop_map(|cs| cs.into_iter().collect::<String>().into_bytes())
}

/// Is `k` one of the count digits (`1`/`2`/`3`) the alphabet offers?
fn is_count_digit(k: &KeyEvent) -> bool {
    matches!(k.code, KeyCode::Char('1' | '2' | '3')) && k.modifiers == KeyModifiers::NONE
}

/// Bound the *count* the fuzzer can build so a single command cannot allocate gigabytes. Counts compose
/// digit-by-digit (`saturating_mul(10)` in the engine), so a random run of digit keys — e.g. `3333333` —
/// builds a huge count. A big count then feeds ONE atomic op (`Command::Paste { count }` multiplies the
/// register `count` times in a single command; `N.`/`Ni…<Esc>` build a `count`-long replay `Vec<Command>`),
/// which balloons to GB *inside that one op*, before `run_session`'s per-key buffer guard can react. This is
/// a harness-input problem, not a product defect — large counts are legitimate Vim. Dropping every digit that
/// immediately follows another digit collapses each run to a SINGLE digit, capping any count at 3. That still
/// exercises counted compositions (`3w`, `2dd`, `3p`, `2.`); multi-digit count *parsing* is the input
/// engine's own property test, not this composed-session fuzzer's job.
fn clamp_count_runs(mut keys: Vec<KeyEvent>) -> Vec<KeyEvent> {
    let mut prev_was_digit = false;
    keys.retain(|k| {
        let digit = is_count_digit(k);
        let keep = !(digit && prev_was_digit);
        prev_was_digit = digit;
        keep
    });
    keys
}

/// Apply whatever the engine emitted, exactly as `main.rs::run` routes it (minus the frontend-only
/// ex/window commands, which this single-window fuzzer does not drive).
fn apply_feed(st: &mut EditorState, out: Feed) {
    match out {
        Feed::Cmd(cmd) => {
            apply_command(st, &cmd);
        }
        Feed::Replay(cmds) => {
            for c in &cmds {
                apply_command(st, c);
            }
        }
        Feed::ExecuteEx(_)
        | Feed::CmdlineInsertUnder { .. }
        | Feed::FilterMotion { .. }
        | Feed::Pending
        | Feed::Ignored => {}
    }
}

/// Upper bound on the fuzzed buffer size, enforced by `run_session`. Yank-whole-buffer -> paste is
/// legitimate, unbounded Vim behavior (`ggVGyp` / `ggyGp` doubles the buffer every repeat), so a random
/// session that happens to compose it repeatedly grows the buffer *exponentially* — reaching multiple GB
/// within the 140-key budget and OOMing/hanging CI on the unlucky seeds. This is a harness-input problem,
/// not a product defect: we must NOT cripple the product's paste. Instead, once the buffer crosses this
/// cap we stop feeding further keys for that case; the round-trip invariant is still exercised in full over
/// every op applied so far (which is where the composition coverage lives — size adds nothing). 256 KiB is
/// orders of magnitude above any interesting composition, yet small enough that the whole per-case
/// undo/redo round trip stays cheap even when a doubling session runs right up to the cap — keeping the
/// suite fast and seed-independent (a larger cap lets rare seeds spend seconds memcpying multi-MB buffers).
const MAX_FUZZ_BUFFER_BYTES: usize = 256 * 1024;

/// Drive `keys` through a fresh engine+state, asserting the per-keystroke UTF-8 / cursor-boundary
/// invariants along the way, and return the final state (Normal mode, any insert session closed).
fn run_session(init: &[u8], keys: &[KeyEvent]) -> Result<EditorState, TestCaseError> {
    let mut engine = InputEngine::new();
    let mut st = EditorState::new(init.to_vec());
    for key in keys {
        let out = engine.feed(*key, st.mode());
        apply_feed(&mut st, out);

        let s = std::str::from_utf8(st.bytes());
        prop_assert!(s.is_ok(), "buffer became invalid UTF-8 after {key:?}");
        let s = s.expect("just checked ok");
        prop_assert!(
            st.cursor() <= s.len(),
            "cursor {} past end {} after {key:?}",
            st.cursor(),
            s.len()
        );
        prop_assert!(
            s.is_char_boundary(st.cursor()),
            "cursor {} off a char boundary after {key:?} (buffer {s:?})",
            st.cursor()
        );

        // Bound exponential yank+paste growth (see `MAX_FUZZ_BUFFER_BYTES`): the invariants above have been
        // checked for this key; once the buffer is huge we stop feeding further keys rather than let a
        // doubling session balloon to gigabytes. The round-trip is still exercised over every applied op.
        if st.bytes().len() > MAX_FUZZ_BUFFER_BYTES {
            break;
        }
    }
    // Close any open insert/visual session so a dangling change is committed to undo history.
    let esc = engine.feed(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), st.mode());
    apply_feed(&mut st, esc);
    Ok(st)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(600))]

    /// ANY session (including `u`/CTRL-R): per-key UTF-8 + cursor-boundary, and a full undo terminates
    /// and restores the initial buffer. `undo-all -> root` is valid however the session branched history.
    #[test]
    fn random_keystroke_session_preserves_core_invariants(
        init in text(),
        keys in prop::collection::vec(proptest::sample::select(alphabet(true)), 0..140)
            .prop_map(clamp_count_runs),
    ) {
        let mut st = run_session(&init, &keys)?;

        let mut guard = 0u32;
        while st.doc.can_undo() {
            apply_command(&mut st, &Command::Undo);
            guard += 1;
            prop_assert!(guard < 100_000, "undo did not terminate");
        }
        prop_assert_eq!(
            st.bytes(),
            init.as_slice(),
            "full undo of a keystroke session must restore the initial buffer"
        );
    }

    /// A MONOTONIC session (no in-session undo/redo): the full round trip holds — undo-all -> initial,
    /// then redo-all -> the edited buffer. Ill-posed with in-session undos (see module docs), hence the
    /// restricted alphabet.
    #[test]
    fn monotonic_session_undo_redo_round_trips(
        init in text(),
        keys in prop::collection::vec(proptest::sample::select(alphabet(false)), 0..140)
            .prop_map(clamp_count_runs),
    ) {
        let mut st = run_session(&init, &keys)?;
        let edited = st.bytes().to_vec();

        let mut guard = 0u32;
        while st.doc.can_undo() {
            apply_command(&mut st, &Command::Undo);
            guard += 1;
            prop_assert!(guard < 100_000, "undo did not terminate");
        }
        prop_assert_eq!(st.bytes(), init.as_slice(), "full undo must restore the initial buffer");

        guard = 0;
        while st.doc.can_redo() {
            apply_command(&mut st, &Command::Redo);
            guard += 1;
            prop_assert!(guard < 100_000, "redo did not terminate");
        }
        prop_assert_eq!(st.bytes(), edited.as_slice(), "full redo must restore the edited buffer");
    }
}
