//! Parity comparison harness — replays the Neovim fixture corpus through ruse's REAL input engine +
//! core and reports, per fixture, whether ruse's observable state matches the pinned oracle.
//!
//! RATCHET (#883): a divergence is allowed only when the fixture is named in the committed
//! `tests/parity/vim/known-divergences.txt`. The test fails on any other divergent fixture (a regression
//! or an unmatched new fixture) and on a listed fixture that now verifies, so the list can only shrink.
//! See `parity_ratchet/mod.rs`. The oracle's own non-corruption gate is `oracle.py --selftest`.
//!
//! The corpus is `tests/parity/vim/fixtures/corpus.yaml` — JSON-in-YAML (a subset of YAML 1.2),
//! captured by `tools/parity/oracle.py`. Run with `-- --nocapture` to see the tally.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ruse_core::{apply_command, EditorState};
use ruse_tui::input::{Feed, InputEngine};
use serde_json::Value;

mod parity_ratchet;

/// Tokenize a fixture key string into key events, matching `nvim_replace_termcodes` for the few
/// special tokens the corpus uses (`<Esc>`, `<CR>`, `<BS>`, `<Tab>`, `<Space>`). Everything else is a
/// literal character key. Case-insensitive on the token name, as Vim's notation is.
fn tokenize(keys: &str) -> Vec<KeyEvent> {
    let ev = |c: KeyCode| KeyEvent::new(c, KeyModifiers::NONE);
    let mut out = Vec::new();
    let chars: Vec<char> = keys.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '<' {
            if let Some(end) = chars[i + 1..].iter().position(|&c| c == '>') {
                let name: String = chars[i + 1..i + 1 + end].iter().collect();
                let lower = name.to_ascii_lowercase();
                let code = match lower.as_str() {
                    "esc" => Some(KeyCode::Esc),
                    "cr" | "enter" => Some(KeyCode::Enter),
                    "bs" => Some(KeyCode::Backspace),
                    "tab" => Some(KeyCode::Tab),
                    "space" => Some(KeyCode::Char(' ')),
                    _ => None,
                };
                if let Some(code) = code {
                    out.push(ev(code));
                    i += end + 2;
                    continue;
                }
                // `<C-x>` — a CTRL-modified key (e.g. `<C-o>`, `<C-g>`), matching nvim_replace_termcodes.
                // The name is `c-<char>`; emit the base char with the CONTROL modifier so the input
                // engine's ctrl checks (CTRL-O one-shot, CTRL-G undo-break) fire.
                if let Some(rest) = lower.strip_prefix("c-") {
                    let rc: Vec<char> = rest.chars().collect();
                    if rc.len() == 1 {
                        out.push(KeyEvent::new(KeyCode::Char(rc[0]), KeyModifiers::CONTROL));
                        i += end + 2;
                        continue;
                    }
                }
            }
        }
        out.push(ev(KeyCode::Char(chars[i])));
        i += 1;
    }
    out
}

/// Drive ruse: feed each key through the input engine at the current mode, applying any completed
/// command to the core. This mirrors the frontend loop in `main.rs`.
fn drive_ruse(lines: &[String], keys: &str) -> EditorState {
    let mut st = EditorState::new(lines.join("\n").into_bytes());
    let mut engine = InputEngine::new();
    let events = tokenize(keys);
    let mut i = 0;
    while i < events.len() {
        let key = events[i];
        let mode = st.mode();
        match engine.feed(key, mode) {
            Feed::Cmd(cmd) => {
                apply_command(&mut st, &cmd);
            }
            // `.` (dot-repeat) expands to the recorded change's command list; apply each in turn.
            Feed::Replay(cmds) => {
                for cmd in cmds {
                    apply_command(&mut st, &cmd);
                }
            }
            // `/` opens the command-line namespace (F-026); the engine now OWNS the pattern buffer, so
            // the subsequent pattern chars and the terminating `<CR>` are fed straight through
            // `engine.feed` — the Enter yields the folded `Feed::Cmd(Search…)` (or `Feed::ExecuteEx`
            // for a `:`-line, of which the corpus has none). No manual pattern collection needed.
            Feed::ExecuteEx(_)
            | Feed::CmdlineInsertUnder { .. }
            | Feed::FilterMotion { .. }
            | Feed::Pending
            | Feed::Ignored => {}
        }
        i += 1;
    }
    st
}

/// ruse's byte-offset cursor as Neovim's (row 1-based, col 0-based byte) — the oracle's cursor shape.
fn ruse_cursor(bytes: &[u8], off: usize) -> (i64, i64) {
    let off = off.min(bytes.len());
    let mut row = 1i64;
    let mut line_start = 0usize;
    for (i, &b) in bytes.iter().enumerate().take(off) {
        if b == b'\n' {
            row += 1;
            line_start = i + 1;
        }
    }
    (row, (off - line_start) as i64)
}

fn ruse_text(st: &EditorState) -> Vec<String> {
    st.as_str()
        .expect("fixture buffers are valid UTF-8")
        .split('\n')
        .map(str::to_string)
        .collect()
}

fn reg_shape(r: &ruse_core::Register) -> (String, String) {
    let ty = if r.is_empty() {
        ""
    } else if r.is_linewise() {
        "linewise"
    } else if r.is_blockwise() {
        "blockwise"
    } else {
        "charwise"
    };
    (
        String::from_utf8_lossy(r.text()).into_owned(),
        ty.to_string(),
    )
}

fn ruse_reg(st: &EditorState) -> (String, String) {
    reg_shape(st.register())
}

fn ruse_reg0(st: &EditorState) -> (String, String) {
    reg_shape(st.registers().yank0())
}

fn as_str_vec(v: &Value) -> Vec<String> {
    v.as_array()
        .expect("array")
        .iter()
        .map(|s| s.as_str().expect("string").to_string())
        .collect()
}

/// Replay one fixture through ruse and list every observable that differs from the oracle's `expect`.
fn fixture_diffs(fx: &Value) -> Vec<String> {
    let keys = fx["keys"].as_str().expect("keys");
    let lines = as_str_vec(&fx["lines"]);
    let expect = &fx["expect"];

    let st = drive_ruse(&lines, keys);

    // ruse observables.
    let r_text = ruse_text(&st);
    let (r_row, r_col) = ruse_cursor(st.bytes(), st.cursor());
    let (r_reg_text, r_reg_type) = ruse_reg(&st);
    let (r_reg0_text, r_reg0_type) = ruse_reg0(&st);

    // oracle observables.
    let e_text = as_str_vec(&expect["text"]);
    let e_cur = expect["cursor"].as_array().expect("cursor");
    let (e_row, e_col) = (
        e_cur[0].as_i64().expect("row"),
        e_cur[1].as_i64().expect("col"),
    );
    let e_reg_text = expect["reg_unnamed"]["text"].as_str().expect("reg text");
    let e_reg_type = expect["reg_unnamed"]["type"].as_str().expect("reg type");
    let e_reg0_text = expect["reg0"]["text"].as_str().expect("reg0 text");
    let e_reg0_type = expect["reg0"]["type"].as_str().expect("reg0 type");

    let mut diffs: Vec<String> = Vec::new();
    if r_text != e_text {
        diffs.push(format!("text {r_text:?} != {e_text:?}"));
    }
    if (r_row, r_col) != (e_row, e_col) {
        diffs.push(format!("cursor [{r_row},{r_col}] != [{e_row},{e_col}]"));
    }
    if (r_reg_text.as_str(), r_reg_type.as_str()) != (e_reg_text, e_reg_type) {
        diffs.push(format!(
            "reg {:?}/{} != {:?}/{}",
            r_reg_text, r_reg_type, e_reg_text, e_reg_type
        ));
    }
    if (r_reg0_text.as_str(), r_reg0_type.as_str()) != (e_reg0_text, e_reg0_type) {
        diffs.push(format!(
            "reg0 {:?}/{} != {:?}/{}",
            r_reg0_text, r_reg0_type, e_reg0_text, e_reg0_type
        ));
    }
    diffs
}

fn load_corpus() -> Value {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/parity/vim/fixtures/corpus.yaml"
    );
    let raw = std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("cannot read fixture corpus {path}: {e}"));
    serde_json::from_str(&raw).expect("corpus is JSON-in-YAML")
}

#[test]
fn parity_ruse_vs_neovim_oracle() {
    let corpus = load_corpus();

    let oracle_version = corpus["oracle"]["nvim_version"].as_str().unwrap_or("?");
    let fixtures = corpus["fixtures"].as_array().expect("fixtures array");
    assert!(!fixtures.is_empty(), "corpus has no fixtures");

    println!("\n=== ruse vs neovim parity ({oracle_version}) ===");
    println!("compared observables: text, cursor, reg_unnamed, reg0 (the yank register)\n");

    let mut verified = 0usize;
    let mut divergent_names: Vec<String> = Vec::new();

    for fx in fixtures {
        let name = fx["name"].as_str().expect("name");
        let keys = fx["keys"].as_str().expect("keys");
        let diffs = fixture_diffs(fx);

        if diffs.is_empty() {
            verified += 1;
            println!("  VERIFIED  {name:<22} keys={keys:?}");
        } else {
            divergent_names.push(name.to_string());
            println!("  DIVERGENT {name:<22} keys={keys:?}");
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
        println!("divergent: {}", divergent_names.join(", "));
    }

    let names: Vec<&str> = fixtures
        .iter()
        .map(|fx| fx["name"].as_str().expect("name"))
        .collect();
    parity_ratchet::enforce(
        concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../tests/parity/vim/known-divergences.txt"
        ),
        &names,
        &divergent_names,
    );
}

/// Negative self-test: the comparator must notice a planted mismatch, or the ratchet guards nothing.
#[test]
fn comparator_detects_a_planted_mismatch() {
    let corpus = load_corpus();
    let fx = corpus["fixtures"]
        .as_array()
        .expect("fixtures array")
        .iter()
        .find(|fx| fixture_diffs(fx).is_empty())
        .expect("at least one fixture verifies")
        .clone();
    let mut text = fx.clone();
    text["expect"]["text"][0] = Value::from("planted mismatch");
    assert!(
        !fixture_diffs(&text).is_empty(),
        "a wrong text went unnoticed"
    );
    let mut cursor = fx.clone();
    let row = cursor["expect"]["cursor"][0].as_i64().expect("row");
    cursor["expect"]["cursor"][0] = Value::from(row + 1);
    assert!(
        !fixture_diffs(&cursor).is_empty(),
        "a wrong cursor went unnoticed"
    );
    let mut reg = fx;
    reg["expect"]["reg_unnamed"]["text"] = Value::from("planted mismatch");
    assert!(
        !fixture_diffs(&reg).is_empty(),
        "a wrong register went unnoticed"
    );
}

#[test]
fn ratchet_rejects_new_stale_and_unknown_entries() {
    let fixtures = ["a", "b", "c"];
    let allowed = parity_ratchet::parse("a # known\n\n# comment\n").expect("parses");
    assert_eq!(allowed, vec!["a".to_string()]);
    assert!(parity_ratchet::check(&allowed, &fixtures, &["a".into()]).is_ok());
    let new = parity_ratchet::check(&allowed, &fixtures, &["a".into(), "b".into()]).unwrap_err();
    assert!(new.contains("NEW DIVERGENCE `b`"), "{new}");
    let stale = parity_ratchet::check(&allowed, &fixtures, &[]).unwrap_err();
    assert!(stale.contains("STALE ENTRY `a`"), "{stale}");
    let unknown = parity_ratchet::check(&["z".into()], &fixtures, &[]).unwrap_err();
    assert!(unknown.contains("UNKNOWN ENTRY `z`"), "{unknown}");
    assert!(
        parity_ratchet::parse("a\na\n").is_err(),
        "duplicates are rejected"
    );
}
