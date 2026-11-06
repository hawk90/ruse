//! Parity comparison harness — replays the Neovim fixture corpus through ruse's REAL key path (the
//! headless session over `Session::on_key`, #895) and reports, per fixture, whether ruse's observable state matches the pinned oracle.
//!
//! RATCHET (#883): a divergence is allowed only when the fixture is named in the committed
//! `tests/parity/vim/known-divergences.txt`. The test fails on any other divergent fixture (a regression
//! or an unmatched new fixture) and on a listed fixture that now verifies, so the list can only shrink.
//! See `parity_ratchet/mod.rs`. The oracle's own non-corruption gate is `oracle.py --selftest`.
//!
//! The corpus is `tests/parity/vim/fixtures/corpus.yaml` — JSON-in-YAML (a subset of YAML 1.2),
//! captured by `tools/parity/oracle.py`. Run with `-- --nocapture` to see the tally.

use ruse_tui::headless::Headless;
use serde_json::Value;

mod parity_ratchet;

/// Drive ruse: open the fixture's lines in a headless session and type the fixture's keys (Vim notation,
/// as `nvim_replace_termcodes` reads it) through the real key path: the session's intercepts, the input
/// engine and the Ex dispatch, exactly as the binary routes them (#895). The oracle runs `-u NONE`, so the
/// session uses Vim's factory search case, not ruse's shipped smartcase.
///
/// A fixture whose last line is empty is opened as a newline-TERMINATED buffer (`["", "", ""]` is
/// `"\n\n\n"`): joined bare, its empty last line would be the slot after a final newline, which is not a
/// line in ruse's model (as in Vim's), so the buffer would have a line fewer than nvim's (#1089).
///
/// A fixture with a `file` opens its buffer under that name, so the file's type applies (#1142).
///
/// A `bytes` fixture (#1232) spells each byte of its lines as one Latin-1 char, so a line can hold bytes
/// that are not UTF-8 ([`latin1_bytes`]); its text and registers are read back the same way.
fn drive_ruse(lines: &[String], keys: &str, file: Option<&str>, raw: bool) -> Headless {
    let text = ruse_input(lines);
    let h = match file {
        _ if raw => Headless::vim_bytes(&latin1_bytes(&text)),
        Some(file) => Headless::vim_named(&text, file),
        None => Headless::vim(&text),
    };
    let mut h = h.vanilla_search_case();
    h.keys(keys);
    h
}

/// Whether the fixture opens as a terminated buffer (see [`drive_ruse`]).
fn terminated(lines: &[String]) -> bool {
    lines.len() > 1 && lines.last().is_some_and(String::is_empty)
}

fn ruse_input(lines: &[String]) -> String {
    let mut text = lines.join("\n");
    if terminated(lines) {
        text.push('\n');
    }
    text
}

/// A `bytes` fixture's Latin-1 spelling as the bytes it stands for: one byte per char.
fn latin1_bytes(s: &str) -> Vec<u8> {
    s.chars()
        .map(|c| u8::try_from(u32::from(c)).expect("a bytes fixture spells bytes in Latin-1"))
        .collect()
}

/// Bytes in a `bytes` fixture's spelling: one Latin-1 char per byte.
fn latin1_string(b: &[u8]) -> String {
    b.iter().map(|&c| char::from(c)).collect()
}

/// ruse's buffer as the oracle's lines. A terminated fixture's final newline ends its last line.
fn ruse_text(h: &Headless, terminated: bool, raw: bool) -> Vec<String> {
    let text = if raw {
        latin1_string(&h.bytes())
    } else {
        h.text()
    };
    let text = if terminated {
        text.strip_suffix('\n').unwrap_or(&text)
    } else {
        &text
    };
    text.split('\n').map(str::to_string).collect()
}

/// ruse's register `name` as the oracle's `(text, type)`.
fn ruse_reg(h: &mut Headless, name: char, raw: bool) -> (String, String) {
    if raw {
        let (bytes, kind) = h.register_bytes(name);
        return (latin1_string(&bytes), kind.to_string());
    }
    let (text, kind) = h.register(name);
    (text, kind.to_string())
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

    let raw = fx["bytes"].as_bool().unwrap_or(false);
    let mut h = drive_ruse(&lines, keys, fx["file"].as_str(), raw);

    // ruse observables.
    let r_text = ruse_text(&h, terminated(&lines), raw);
    let (r_row, r_col) = h.cursor_row_col();
    let (r_row, r_col) = (r_row as i64, r_col as i64);
    let (r_reg_text, r_reg_type) = ruse_reg(&mut h, '"', raw);
    let (r_reg0_text, r_reg0_type) = ruse_reg(&mut h, '0', raw);
    let r_emsg = h.emsg().to_string();

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
    // The E-number of the last error the keys raised (#901); `failed` (a beep) is captured, not compared.
    let e_emsg = expect["emsg"].as_str().expect("emsg");

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
    if r_emsg != e_emsg {
        diffs.push(format!("emsg {r_emsg:?} != {e_emsg:?}"));
    }
    // Per-line fold state, recorded only for the fixtures about folds (#903).
    if let Some(e_folds) = expect.get("folds") {
        let e_folds: Vec<Vec<i64>> = e_folds
            .as_array()
            .expect("folds")
            .iter()
            .map(|l| {
                l.as_array()
                    .expect("fold line")
                    .iter()
                    .map(|n| n.as_i64().expect("fold number"))
                    .collect()
            })
            .collect();
        let r_folds: Vec<Vec<i64>> = h.fold_lines().iter().map(|l| l.to_vec()).collect();
        if r_folds != e_folds {
            diffs.push(format!("folds {r_folds:?} != {e_folds:?}"));
        }
    }
    // The window's first line (1-based), recorded only for the fixtures about the view (#1141).
    if let Some(e_top) = expect.get("topline").and_then(Value::as_i64) {
        let r_top = h.topline() as i64 + 1;
        if r_top != e_top {
            diffs.push(format!("topline {r_top} != {e_top}"));
        }
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
    println!(
        "compared observables: text, cursor, reg_unnamed, reg0 (the yank register), emsg (the last E-number), \
         folds (per line, where recorded), topline (where recorded)\n"
    );

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
    let mut reg = fx.clone();
    reg["expect"]["reg_unnamed"]["text"] = Value::from("planted mismatch");
    assert!(
        !fixture_diffs(&reg).is_empty(),
        "a wrong register went unnoticed"
    );
    let mut emsg = fx;
    let planted = if emsg["expect"]["emsg"] == "E999" {
        ""
    } else {
        "E999"
    };
    emsg["expect"]["emsg"] = Value::from(planted);
    assert!(
        !fixture_diffs(&emsg).is_empty(),
        "a wrong error went unnoticed"
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
