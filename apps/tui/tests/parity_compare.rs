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
fn drive_ruse(lines: &[String], keys: &str) -> Headless {
    let mut h = Headless::vim(&lines.join("\n")).vanilla_search_case();
    h.keys(keys);
    h
}

fn ruse_text(h: &Headless) -> Vec<String> {
    h.text().split('\n').map(str::to_string).collect()
}

/// ruse's register `name` as the oracle's `(text, type)`.
fn ruse_reg(h: &mut Headless, name: char) -> (String, String) {
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

    let mut h = drive_ruse(&lines, keys);

    // ruse observables.
    let r_text = ruse_text(&h);
    let (r_row, r_col) = h.cursor_row_col();
    let (r_row, r_col) = (r_row as i64, r_col as i64);
    let (r_reg_text, r_reg_type) = ruse_reg(&mut h, '"');
    let (r_reg0_text, r_reg0_type) = ruse_reg(&mut h, '0');

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
