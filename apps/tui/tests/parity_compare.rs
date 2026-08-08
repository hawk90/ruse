//! Parity comparison harness — replays the Neovim fixture corpus through ruse's REAL input engine +
//! core and reports, per fixture, whether ruse's observable state matches the pinned oracle.
//!
//! CONTRACT (spec/parity/upstreams.yaml): a divergence is a FINDING, not a test failure. ruse is
//! early and many ops legitimately differ from Vim; the value of this harness is the verified/divergent
//! TALLY, not a green/red assertion per op. So this test hard-asserts ONLY that the harness ran (the
//! corpus loaded, every fixture drove without panicking). Whether ruse == nvim is printed, never
//! asserted. The oracle's own non-corruption gate (`oracle.py --selftest`) is the only hard gate.
//!
//! The corpus is `tests/parity/vim/fixtures/corpus.yaml` — JSON-in-YAML (a subset of YAML 1.2),
//! captured by `tools/parity/oracle.py`. Run with `-- --nocapture` to see the tally.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ruse_core::{apply_command, Command, EditorState};
use ruse_tui::input::{Feed, InputEngine};
use serde_json::Value;

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
                let code = match name.to_ascii_lowercase().as_str() {
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
            // `/` opens the search minibuffer. The real frontend (main.rs) collects the typed pattern
            // in a command line — raw keystrokes the input engine does NOT parse — until `<CR>`, then
            // `engine.set_last_search(pattern)` (so a later `n`/`N` repeats it) and applies
            // `Command::SearchNext(pattern)` to move the cursor to the match. Mirror that exactly:
            // consume the SUBSEQUENT keys as raw pattern chars (not via `engine.feed`) up to the
            // terminating Enter, then resume normal feeding. `?` (backward search) is not wired to the
            // engine at all — it never yields `OpenSearch` — so only `/` needs this path.
            Feed::OpenSearch => {
                let mut pattern = String::new();
                i += 1;
                while i < events.len() {
                    match events[i].code {
                        KeyCode::Enter => break, // `<CR>` submits the search line
                        KeyCode::Char(c) => pattern.push(c),
                        KeyCode::Backspace => {
                            pattern.pop();
                        }
                        _ => {} // Esc/other: the corpus never types these mid-pattern
                    }
                    i += 1;
                }
                engine.set_last_search(pattern.clone());
                if !pattern.is_empty() {
                    apply_command(&mut st, &Command::SearchNext(pattern));
                }
            }
            // The ex-line minibuffer (`:`) is a separate concern (no ex fixtures in the corpus);
            // pending/ignored are no-ops.
            Feed::OpenExLine | Feed::Pending | Feed::Ignored => {}
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

fn ruse_reg(st: &EditorState) -> (String, String) {
    let r = st.register();
    let ty = if r.is_empty() {
        ""
    } else if r.is_linewise() {
        "linewise"
    } else {
        "charwise"
    };
    (
        String::from_utf8_lossy(r.text()).into_owned(),
        ty.to_string(),
    )
}

fn as_str_vec(v: &Value) -> Vec<String> {
    v.as_array()
        .expect("array")
        .iter()
        .map(|s| s.as_str().expect("string").to_string())
        .collect()
}

#[test]
fn parity_ruse_vs_neovim_oracle() {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/parity/vim/fixtures/corpus.yaml"
    );
    let raw = std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("cannot read fixture corpus {path}: {e}"));
    let corpus: Value = serde_json::from_str(&raw).expect("corpus is JSON-in-YAML");

    let oracle_version = corpus["oracle"]["nvim_version"].as_str().unwrap_or("?");
    let fixtures = corpus["fixtures"].as_array().expect("fixtures array");
    assert!(!fixtures.is_empty(), "corpus has no fixtures");

    println!("\n=== ruse vs neovim parity ({oracle_version}) ===");
    println!("compared observables: text, cursor, reg_unnamed  (reg0 excluded: ruse has no yank register)\n");

    let mut verified = 0usize;
    let mut divergent_names: Vec<String> = Vec::new();

    for fx in fixtures {
        let name = fx["name"].as_str().expect("name");
        let keys = fx["keys"].as_str().expect("keys");
        let lines = as_str_vec(&fx["lines"]);
        let expect = &fx["expect"];

        let st = drive_ruse(&lines, keys);

        // ruse observables.
        let r_text = ruse_text(&st);
        let (r_row, r_col) = ruse_cursor(st.bytes(), st.cursor());
        let (r_reg_text, r_reg_type) = ruse_reg(&st);

        // oracle observables.
        let e_text = as_str_vec(&expect["text"]);
        let e_cur = expect["cursor"].as_array().expect("cursor");
        let (e_row, e_col) = (
            e_cur[0].as_i64().expect("row"),
            e_cur[1].as_i64().expect("col"),
        );
        let e_reg_text = expect["reg_unnamed"]["text"].as_str().expect("reg text");
        let e_reg_type = expect["reg_unnamed"]["type"].as_str().expect("reg type");

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
        println!(
            "divergent (findings, not failures): {}",
            divergent_names.join(", ")
        );
    }

    // The ONLY hard assertion: the harness ran end to end. Divergence is data, not a failure.
    assert_eq!(
        verified + divergent_names.len(),
        total,
        "every fixture must have been compared"
    );
}
