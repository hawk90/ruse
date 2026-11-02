//! Full-stack keystroke FUZZER (testing-and-benchmarks.md §1.2 property + §1.9 e2e): the layer the
//! micro suites miss. The oracle corpus checks short single ops; the core property tests fuzz `Command`
//! streams; the input-engine property test fuzzes random (key, mode) pairs. NONE of them drive the
//! COMPOSED stack — keystroke -> the session's real key path (`Session::on_key` through the headless
//! harness, #895: macro layer, intercepts, input engine, Ex dispatch) -> Command -> edit -> undo — over a
//! long realistic session, which is exactly
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
use ruse_core::{Command, Workspace};
use ruse_tui::headless::Headless;

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

/// Drive `keys` through a fresh headless session, asserting the per-keystroke UTF-8 / cursor-boundary
/// invariants along the way, and return the final session (Normal mode, any insert session closed). A
/// random `ZZ`/`ZQ` may end the session early; the keys after it are then ignored, as in the binary.
fn run_session(init: &[u8], keys: &[KeyEvent]) -> Result<Headless, TestCaseError> {
    let init = std::str::from_utf8(init).expect("the generator builds UTF-8");
    let mut h = Headless::vim(init);
    for key in keys {
        h.key(*key);
        let ws = h.workspace();
        let (bytes, cursor) = (ws.focused().doc.bytes(), ws.focused().view.cursor());

        let s = std::str::from_utf8(bytes);
        prop_assert!(s.is_ok(), "buffer became invalid UTF-8 after {key:?}");
        let s = s.expect("just checked ok");
        prop_assert!(
            cursor <= s.len(),
            "cursor {} past end {} after {key:?}",
            cursor,
            s.len()
        );
        prop_assert!(
            s.is_char_boundary(cursor),
            "cursor {} off a char boundary after {key:?} (buffer {s:?})",
            cursor
        );

        // Bound exponential yank+paste growth (see `MAX_FUZZ_BUFFER_BYTES`): the invariants above have been
        // checked for this key; once the buffer is huge we stop feeding further keys rather than let a
        // doubling session balloon to gigabytes. The round-trip is still exercised over every applied op.
        if bytes.len() > MAX_FUZZ_BUFFER_BYTES {
            break;
        }
    }
    // Close any open insert/visual session so a dangling change is committed to undo history.
    h.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    Ok(h)
}

/// Apply `cmd` to the core until `more` says the history is exhausted; `None` if it does not terminate.
fn exhaust(ws: &mut Workspace, cmd: &Command, more: fn(&Workspace) -> bool) -> Option<()> {
    for _ in 0..100_000 {
        if !more(ws) {
            return Some(());
        }
        ws.apply(cmd);
    }
    None
}

fn can_undo(ws: &Workspace) -> bool {
    ws.focused().doc.can_undo()
}

fn can_redo(ws: &Workspace) -> bool {
    ws.focused().doc.can_redo()
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
        let mut h = run_session(&init, &keys)?;
        let ws = h.workspace_mut();

        prop_assert!(exhaust(ws, &Command::Undo, can_undo).is_some(), "undo did not terminate");
        prop_assert_eq!(
            ws.focused().doc.bytes(),
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
        let mut h = run_session(&init, &keys)?;
        let ws = h.workspace_mut();
        let edited = ws.focused().doc.bytes().to_vec();

        prop_assert!(exhaust(ws, &Command::Undo, can_undo).is_some(), "undo did not terminate");
        prop_assert_eq!(ws.focused().doc.bytes(), init.as_slice(), "full undo must restore the initial buffer");

        prop_assert!(exhaust(ws, &Command::Redo, can_redo).is_some(), "redo did not terminate");
        prop_assert_eq!(ws.focused().doc.bytes(), edited.as_slice(), "full redo must restore the edited buffer");
    }
}
