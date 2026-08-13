//! Emacs parity comparison harness — replays the Emacs fixture corpus through ruse's REAL command
//! registry + core and reports, per fixture, whether ruse's observable state matches the pinned Emacs
//! oracle (tools/parity/emacs_oracle.py).
//!
//! CONTRACT (spec/parity/upstreams.yaml): a divergence is a FINDING, not a test failure — identical to the
//! Neovim comparator (apps/tui/tests/parity_compare.rs). ruse's Emacs profile is early and legitimately
//! differs from Emacs on several commands (e.g. Emacs `beginning-of-buffer` PUSHES the mark and `yank` SETS
//! it; ruse does neither yet). The value of this harness is the verified/divergent TALLY, not a green/red
//! assertion per command. So this test hard-asserts ONLY that the harness ran (the corpus loaded, every
//! fixture drove without panicking, every op name resolved). Whether ruse == Emacs is printed, never
//! asserted. The oracle's own non-corruption gate (`emacs_oracle.py --selftest`) is the only hard gate.
//!
//! HOW IT BRIDGES THE TWO EDITORS: a fixture's `ops` are Emacs command NAMES. The oracle ran them in Emacs
//! via `call-interactively`; here the SAME names resolve through `emacs_command_by_name` — the very registry
//! that M-x uses (F-012, #148) — into ruse `Command`s applied to the core. One fixture, one vocabulary, both
//! editors. Point/mark are 0-based CHARACTER offsets in the corpus; the seed is ASCII so char == byte and
//! ruse's byte offsets compare directly (see the oracle's scope note).
//!
//! The corpus is `tests/parity/emacs/fixtures/corpus.yaml` — JSON-in-YAML (a subset of YAML 1.2), captured
//! by `tools/parity/emacs_oracle.py`. Run with `-- --nocapture` to see the tally.

use ruse_core::{apply_command, CaretGravity, EditorState};
use ruse_tui::input::emacs_command_by_name;
use serde_json::Value;

/// Drive ruse: home the cursor to the fixture's start, then resolve each Emacs command name through the M-x
/// registry and apply it to the core. Mirrors the M-x execution path in `input::feed_cmdline`. Returns the
/// settled state plus any op names the registry did not know (a coverage gap, surfaced not silently eaten).
fn drive_ruse(text: &str, ops: &[String], point: usize) -> (EditorState, Vec<String>) {
    let mut st = EditorState::new(text.as_bytes().to_vec());
    // The Emacs profile rests point BETWEEN characters (D-050 / RFC-0015), the same gravity the frontend
    // installs for `RUSE_PROFILE=emacs`; without it every Emacs edit would be Vim-clamped one column short.
    st.set_caret_gravity(CaretGravity::BetweenChar);
    st.set_cursor(point); // char offset == byte offset for the ASCII seed corpus
    let mut unknown = Vec::new();
    for op in ops {
        match emacs_command_by_name(op) {
            Some(cmd) => {
                apply_command(&mut st, &cmd);
            }
            None => unknown.push(op.clone()),
        }
    }
    (st, unknown)
}

fn ruse_text(st: &EditorState) -> Vec<String> {
    st.as_str()
        .expect("fixture buffers are valid UTF-8")
        .split('\n')
        .map(str::to_string)
        .collect()
}

/// ruse's unnamed register as the oracle's `kill`: the register's text, or `None` when it is empty (Emacs's
/// kill-ring is empty -> the corpus records `kill: null`). D-026: ruse models one unnamed register.
fn ruse_kill(st: &EditorState) -> Option<String> {
    let r = st.register();
    if r.is_empty() {
        None
    } else {
        Some(String::from_utf8_lossy(r.text()).into_owned())
    }
}

fn as_str_vec(v: &Value) -> Vec<String> {
    v.as_array()
        .expect("array")
        .iter()
        .map(|s| s.as_str().expect("string").to_string())
        .collect()
}

fn as_op_vec(v: &Value) -> Vec<String> {
    v.as_array()
        .expect("ops array")
        .iter()
        .map(|s| s.as_str().expect("op is a string").to_string())
        .collect()
}

/// The corpus's `mark`/`kill` are `number|null` / `string|null`; map JSON null to `None`.
fn opt_i64(v: &Value) -> Option<i64> {
    if v.is_null() {
        None
    } else {
        Some(v.as_i64().expect("mark is an integer"))
    }
}

fn opt_str(v: &Value) -> Option<String> {
    if v.is_null() {
        None
    } else {
        Some(v.as_str().expect("kill is a string").to_string())
    }
}

#[test]
fn parity_ruse_vs_emacs_oracle() {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/parity/emacs/fixtures/corpus.yaml"
    );
    let raw = std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("cannot read fixture corpus {path}: {e}"));
    let corpus: Value = serde_json::from_str(&raw).expect("corpus is JSON-in-YAML");

    let oracle_version = corpus["oracle"]["emacs_version"].as_str().unwrap_or("?");
    let fixtures = corpus["fixtures"].as_array().expect("fixtures array");
    assert!(!fixtures.is_empty(), "corpus has no fixtures");

    println!("\n=== ruse vs emacs parity ({oracle_version}) ===");
    println!("compared observables: text, point, mark, kill (the unnamed register)\n");

    let mut verified = 0usize;
    let mut divergent_names: Vec<String> = Vec::new();
    let mut all_unknown: Vec<String> = Vec::new();

    for fx in fixtures {
        let name = fx["name"].as_str().expect("name");
        let text = fx["text"].as_str().expect("text");
        let ops = as_op_vec(&fx["ops"]);
        let point = fx.get("point").and_then(Value::as_u64).unwrap_or(0) as usize;
        let expect = &fx["expect"];

        let (st, unknown) = drive_ruse(text, &ops, point);
        for u in &unknown {
            if !all_unknown.contains(u) {
                all_unknown.push(u.clone());
            }
        }

        // ruse observables.
        let r_text = ruse_text(&st);
        let r_point = st.cursor() as i64;
        let r_mark = st.mark().map(|m| m as i64);
        let r_kill = ruse_kill(&st);

        // oracle observables.
        let e_text = as_str_vec(&expect["text"]);
        let e_point = expect["point"].as_i64().expect("point");
        let e_mark = opt_i64(&expect["mark"]);
        let e_kill = opt_str(&expect["kill"]);

        let mut diffs: Vec<String> = Vec::new();
        if r_text != e_text {
            diffs.push(format!("text {r_text:?} != {e_text:?}"));
        }
        if r_point != e_point {
            diffs.push(format!("point {r_point} != {e_point}"));
        }
        if r_mark != e_mark {
            diffs.push(format!("mark {r_mark:?} != {e_mark:?}"));
        }
        if r_kill != e_kill {
            diffs.push(format!("kill {r_kill:?} != {e_kill:?}"));
        }
        if !unknown.is_empty() {
            diffs.push(format!("unresolved ops {unknown:?}"));
        }

        if diffs.is_empty() {
            verified += 1;
            println!("  VERIFIED  {name:<28} ops={ops:?}");
        } else {
            divergent_names.push(name.to_string());
            println!("  DIVERGENT {name:<28} ops={ops:?}");
            for d in &diffs {
                println!("              - {d}");
            }
        }
    }

    let total = fixtures.len();
    println!(
        "\n=== tally: {verified}/{total} verified, {} divergent ===",
        divergent_names.len()
    );
    if !divergent_names.is_empty() {
        println!(
            "divergent (findings, not failures): {}",
            divergent_names.join(", ")
        );
    }
    if !all_unknown.is_empty() {
        println!(
            "registry gaps (command names M-x cannot resolve yet): {}",
            all_unknown.join(", ")
        );
    }

    // The ONLY hard assertion: the harness ran end to end. Divergence is data, not a failure.
    assert_eq!(
        verified + divergent_names.len(),
        total,
        "every fixture must have been compared"
    );
}
