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

use ruse_core::{Command, Mode, SelectKind};
use ruse_tui::headless::Headless;
use serde_json::{json, Value};

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

/// The name nvim's `nvim_get_mode().mode` gives ruse's mode after the keys (#900). The oracle feeds the
/// keys with `nvim_feedkeys(.., 'x')`, which ends Insert mode and drops an armed operator once the keys run
/// out, so those read as `n` here too: what is left to compare is Normal, Visual, Select and the command
/// line.
fn ruse_mode(h: &Headless) -> String {
    let kind = |k: SelectKind, c: &str, l: &str, b: &str| match k {
        SelectKind::Charwise => c.to_string(),
        SelectKind::Linewise => l.to_string(),
        SelectKind::Blockwise => b.to_string(),
    };
    match h.mode() {
        Mode::Normal if h.engine().cmdline().is_some() => "c".into(),
        Mode::Normal | Mode::Insert | Mode::Replace | Mode::VirtualReplace => "n".into(),
        Mode::Visual { kind: k } => kind(k, "v", "V", "\u{16}"),
        Mode::Select { kind: k } => kind(k, "s", "S", "\u{13}"),
        Mode::Terminal => "t".into(),
        Mode::TerminalNormal => "nt".into(),
    }
}

/// The name `getregtype()` / `visualmode()` gives a selection kind, as the oracle records it.
fn kind_name(k: SelectKind) -> &'static str {
    match k {
        SelectKind::Charwise => "charwise",
        SelectKind::Linewise => "linewise",
        SelectKind::Blockwise => "blockwise",
    }
}

/// Byte offset `p` of ruse's buffer as the oracle's `[line, col0]`.
fn line_col(bytes: &[u8], p: usize) -> Value {
    let p = p.min(bytes.len());
    let ls = bytes[..p]
        .iter()
        .rposition(|&b| b == b'\n')
        .map_or(0, |i| i + 1);
    let line = bytes[..p].iter().filter(|&&b| b == b'\n').count() + 1;
    json!([line, p - ls])
}

/// ruse's marks as the oracle records them (#900): the set ones of a-z, `<`, `>`, `[`, `]`, `.`, `^`, and
/// the last Visual mode. A linewise area's `'<` is its first line's start and its `'>` its last line at
/// column -1 (nvim's MAXCOL).
fn ruse_marks(h: &Headless) -> (Value, String) {
    let ws = h.workspace();
    let (bytes, view) = (ws.focused().doc.bytes(), &ws.focused().view);
    let mut marks = serde_json::Map::new();
    for c in 'a'..='z' {
        if let Some(p) = view.named_mark(c) {
            marks.insert(c.to_string(), line_col(bytes, p.0));
        }
    }
    let mut vmode = String::new();
    if let Some((s, e, kind)) = view.last_visual_bounds() {
        vmode = kind_name(kind).to_string();
        let (s, mut e) = (line_col(bytes, s.0), line_col(bytes, e.0));
        let mut s = s;
        if kind == SelectKind::Linewise {
            s[1] = json!(0);
            e[1] = json!(-1);
        }
        marks.insert("<".into(), s);
        marks.insert(">".into(), e);
    }
    let rest = [
        ('[', view.change_mark_start()),
        (']', view.change_mark_end()),
        ('.', view.last_change()),
        ('^', view.named_mark('^')),
    ];
    for (c, p) in rest {
        if let Some(p) = p {
            marks.insert(c.to_string(), line_col(bytes, p.0));
        }
    }
    (Value::Object(marks), vmode)
}

/// Whether ruse's mark `got` is the oracle's `want`: the same line, and the same column unless the oracle's
/// is -1 (MAXCOL, a linewise end whose column Vim leaves unset).
fn mark_matches(got: Option<&Value>, want: &Value) -> bool {
    got.is_some_and(|g| g[0] == want[0] && (want[1] == -1 || g[1] == want[1]))
}

/// ruse's text after each undo back to the buffer it opened with, at most 20 (#900), as the oracle walks
/// `:undo` back to where the keys started. Changes the session, so it is read last.
fn ruse_undo_walk(h: &mut Headless, terminated: bool, raw: bool) -> Vec<Vec<String>> {
    let mut out = Vec::new();
    for _ in 0..20 {
        if !h.workspace().focused().doc.can_undo() {
            break;
        }
        let before = h.workspace().focused().doc.revision();
        h.workspace_mut().apply(&Command::Undo);
        if h.workspace().focused().doc.revision() == before {
            break;
        }
        out.push(ruse_text(h, terminated, raw));
    }
    out
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
    // The mode the keys end in (#900), recorded for every fixture.
    if let Some(e_mode) = expect.get("mode").and_then(Value::as_str) {
        let r_mode = ruse_mode(&h);
        if r_mode != e_mode {
            diffs.push(format!("mode {r_mode:?} != {e_mode:?}"));
        }
    }
    // The #900 observables, recorded only for the fixtures that observe them.
    if let Some(e_marks) = expect.get("marks").and_then(Value::as_object) {
        let (r_marks, r_vmode) = ruse_marks(&h);
        let r_marks = r_marks.as_object().expect("marks");
        let mut names: Vec<&String> = e_marks.keys().chain(r_marks.keys()).collect();
        names.sort();
        names.dedup();
        for m in names {
            let ok = match e_marks.get(m) {
                Some(want) => mark_matches(r_marks.get(m), want),
                None => false,
            };
            if !ok {
                diffs.push(format!(
                    "mark '{m} {} != {}",
                    r_marks.get(m).map_or("unset".into(), Value::to_string),
                    e_marks.get(m).map_or("unset".into(), Value::to_string)
                ));
            }
        }
        let e_vmode = expect["vmode"].as_str().unwrap_or("");
        if r_vmode != e_vmode {
            diffs.push(format!("visualmode {r_vmode:?} != {e_vmode:?}"));
        }
    }
    if let Some(e_regs) = expect.get("regs").and_then(Value::as_object) {
        for (name, want) in e_regs {
            let c = name.chars().next().expect("register name");
            let (text, kind) = ruse_reg(&mut h, c, raw);
            let (wt, wk) = (
                want["text"].as_str().unwrap_or(""),
                want["type"].as_str().unwrap_or(""),
            );
            if (text.as_str(), kind.as_str()) != (wt, wk) {
                diffs.push(format!("reg{name} {text:?}/{kind} != {wt:?}/{wk}"));
            }
        }
    }
    if let Some(e_visual) = expect.get("visual") {
        let view = &h.workspace().focused().view;
        let r_visual = match (view.mode(), view.visual_anchor()) {
            (Mode::Visual { kind }, Some(a)) => {
                json!([
                    kind_name(kind),
                    line_col(h.workspace().focused().doc.bytes(), a.0)
                ])
            }
            _ => Value::Null,
        };
        if &r_visual != e_visual {
            diffs.push(format!("visual {r_visual} != {e_visual}"));
        }
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
    // The undo walk changes the session, so it is compared last (#900).
    if let Some(e_undo) = expect.get("undo").and_then(Value::as_array) {
        let e_undo: Vec<Vec<String>> = e_undo.iter().map(as_str_vec).collect();
        let r_undo = ruse_undo_walk(&mut h, terminated(&lines), raw);
        if r_undo != e_undo {
            diffs.push(format!("undo {r_undo:?} != {e_undo:?}"));
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
         mode, and where recorded folds (per line), topline, marks with visualmode, regs (\"1-\"9, \"-), \
         the Visual area and the undo walk\n"
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

    // #900: the mode, a mark, a numbered register, the Visual area and the undo walk.
    let verified = |has: &str| {
        corpus["fixtures"]
            .as_array()
            .expect("fixtures array")
            .iter()
            .find(|fx| {
                let e = &fx["expect"];
                !e[has].is_null()
                    && e[has].as_object().is_none_or(|o| !o.is_empty())
                    && e[has].as_array().is_none_or(|a| !a.is_empty())
                    && fixture_diffs(fx).is_empty()
            })
            .unwrap_or_else(|| panic!("a verified fixture records {has}"))
            .clone()
    };
    let mut mode = verified("mode");
    mode["expect"]["mode"] = Value::from("R");
    assert!(
        !fixture_diffs(&mode).is_empty(),
        "a wrong mode went unnoticed"
    );
    let mut mark = verified("marks");
    let first = mark["expect"]["marks"]
        .as_object()
        .expect("marks")
        .keys()
        .next()
        .expect("a mark")
        .clone();
    mark["expect"]["marks"][&first][0] = Value::from(999);
    assert!(
        !fixture_diffs(&mark).is_empty(),
        "a wrong mark went unnoticed"
    );
    let mut regs = verified("regs");
    regs["expect"]["regs"]["9"]["text"] = Value::from("planted mismatch");
    assert!(
        !fixture_diffs(&regs).is_empty(),
        "a wrong numbered register went unnoticed"
    );
    let mut visual = verified("visual");
    visual["expect"]["visual"] = json!(["linewise", [999, 0]]);
    assert!(
        !fixture_diffs(&visual).is_empty(),
        "a wrong Visual area went unnoticed"
    );
    let mut undo = verified("undo");
    undo["expect"]["undo"][0][0] = Value::from("planted mismatch");
    assert!(
        !fixture_diffs(&undo).is_empty(),
        "a wrong undo walk went unnoticed"
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
