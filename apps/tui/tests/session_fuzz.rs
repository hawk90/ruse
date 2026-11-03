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
    // d c y; find f F t T ; ,; search / n N; register "; dot .; counts 1 2 3; the fold prefix z (with
    // `f`, `o`, `c`, `a`, `d`, `E`, `R`, `M`, `j`, `k` from the set, so closed folds meet every operator,
    // #819); and literal insert chars (so insert sessions produce real bytes: a Latin, a Hangul and a CJK
    // wide char, a combining acute, a zero-width joiner, an emoji and a tab, so wide and composing text
    // meets every operator, #909).
    for c in
        "hjklwbeWBE0$^gGiaoOvVRxXpPJ~DCYsdcyfFtT;,nN/\".123AIqZzM é가漢\u{301}\u{200d}👩\t".chars()
    {
        v.push(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
    }
    for code in [KeyCode::Esc, KeyCode::Enter, KeyCode::Backspace] {
        v.push(KeyEvent::new(code, KeyModifiers::NONE));
    }
    // CTRL-O insert one-shot, CTRL-V blockwise, CTRL-^ Lang-Arg toggle — history-neutral prefixes; CTRL-W
    // (with `s`, `v`, `c`, `w`, `o` already in the set) splits, closes and cycles windows over the one
    // buffer, so the walker sees several views of it (#906).
    for c in ['o', 'v', '^', 'w'] {
        v.push(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL));
    }
    if include_history_ops {
        v.push(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::NONE)); // undo
        v.push(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL)); // redo
    }
    v
}

/// Pieces of hard text for the starting buffer (#909): wide CJK and Hangul, a combining sequence, a ZWJ
/// emoji family, a flag pair, tabs mixed with wide chars, and line breaks.
const HARD: &[&str] = &[
    "가",
    "漢字",
    "e\u{301}",
    "a\u{301}\u{302}",
    "👨\u{200d}👩\u{200d}👧",
    "🇰🇷",
    "\t",
    "\t가\t",
    "\n",
    "a",
    " ",
];

/// A small random UTF-8 starting buffer: arbitrary scalars mixed with [`HARD`] pieces, so the cursor-
/// boundary and cluster cases come up often rather than by luck.
fn text() -> impl Strategy<Value = Vec<u8>> {
    let piece = prop_oneof![
        any::<char>().prop_map(String::from),
        proptest::sample::select(HARD).prop_map(String::from),
    ];
    prop::collection::vec(piece, 0..24).prop_map(|ps| ps.concat().into_bytes())
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

        // The whole-state walker (#906): undo tree, anchors, marks, windows and buffers stay consistent,
        // and so do the folds and their anchors.
        h.assert_invariants();

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

/// One line edit of the fold model property.
#[derive(Clone, Copy, Debug)]
enum LineOp {
    Down,
    Up,
    DeleteLine,
    OpenBelow,
    OpenAbove,
    PutBelow,
    PutAbove,
    Join,
}

impl LineOp {
    fn keys(self) -> &'static str {
        match self {
            LineOp::Down => "j",
            LineOp::Up => "k",
            LineOp::DeleteLine => "dd",
            LineOp::OpenBelow => "onew<Esc>",
            LineOp::OpenAbove => "Onew<Esc>",
            LineOp::PutBelow => "yyp",
            LineOp::PutAbove => "yyP",
            LineOp::Join => "J",
        }
    }

    /// Apply this op at `cur` (the cursor line before it) to the model: per line, whether it is in the
    /// fold. A line made below a fold line joins the fold unless that is the fold's last line, one made
    /// above unless it is the first (nvim: `o` on the last line and `O` on the first open outside it); a
    /// joined-in line is gone.
    fn apply(self, inside: &mut Vec<bool>, cur: usize) {
        let first = inside.iter().position(|&x| x);
        let last = inside.iter().rposition(|&x| x);
        match self {
            LineOp::Down | LineOp::Up => {}
            LineOp::DeleteLine => {
                inside.remove(cur);
                if inside.is_empty() {
                    inside.push(false);
                }
            }
            LineOp::OpenBelow | LineOp::PutBelow => {
                inside.insert(cur + 1, inside[cur] && last != Some(cur));
            }
            LineOp::OpenAbove | LineOp::PutAbove => {
                inside.insert(cur, inside[cur] && first != Some(cur));
            }
            LineOp::Join => {
                if cur + 1 < inside.len() {
                    inside.remove(cur + 1);
                }
            }
        }
    }
}

fn line_op() -> impl Strategy<Value = LineOp> {
    proptest::sample::select(vec![
        LineOp::Down,
        LineOp::Up,
        LineOp::DeleteLine,
        LineOp::OpenBelow,
        LineOp::OpenAbove,
        LineOp::PutBelow,
        LineOp::PutAbove,
        LineOp::Join,
    ])
}

proptest! {
    #![proptest_config(cases(200))]

    /// The fold model property (#819, #903): a manual fold follows line edits as nvim's `foldMarkAdjust`
    /// moves it. The model marks the lines in the fold (see [`LineOp::apply`]); the fold must run from
    /// the first marked line to the last, and be gone once no line is marked. The fold is open, so each
    /// edit acts on one line.
    #[test]
    fn a_manual_fold_follows_line_edits(
        n in 4usize..10,
        a in 0usize..10,
        len in 1usize..5,
        ops in prop::collection::vec(line_op(), 0..24),
    ) {
        let a = a % (n - 1);
        let b = (a + len).min(n - 1);
        let text: String = (0..n).map(|i| format!("a{i}\n")).collect();
        let mut h = Headless::vim(&text);
        h.keys(&format!("{}Gzf{}jzo", a + 1, b - a));
        let mut inside: Vec<bool> = (0..n).map(|i| a <= i && i <= b).collect();
        for op in ops {
            let cur = h.cursor_row_col().0 - 1;
            h.keys(op.keys());
            op.apply(&mut inside, cur);
            h.assert_invariants();
            let first = inside.iter().position(|&x| x);
            let want = first.zip(inside.iter().rposition(|&x| x));
            let got = h.folds().first().map(|f| (f.start, f.end));
            prop_assert_eq!(got, want, "after {:?} on line {} ({:?})", op, cur, h.text());
            prop_assert!(h.folds().len() <= 1);
        }
    }
}

proptest! {
    #![proptest_config(cases(600))]

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
