//! Emacs parity comparison harness — replays the Emacs fixture corpus through ruse's REAL command
//! registry + core and reports, per fixture, whether ruse's observable state matches the pinned Emacs
//! oracle (tools/parity/emacs_oracle.py).
//!
//! RATCHET (#883), identical to the Neovim comparator (apps/tui/tests/parity_compare.rs): a divergence is
//! allowed only when the fixture is named in the committed `tests/parity/emacs/known-divergences.txt`. The
//! test fails on any other divergent fixture (a regression, an unresolved op name, or an unmatched new
//! fixture) and on a listed fixture that now verifies, so the list can only shrink. See
//! `parity_ratchet/mod.rs`. The oracle's own non-corruption gate is `emacs_oracle.py --selftest`.
//!
//! HOW IT BRIDGES THE TWO EDITORS: a fixture's `ops` are Emacs command NAMES. The oracle ran them in Emacs
//! via `call-interactively`; here the SAME names resolve through `emacs_command_by_name` — the very registry
//! that M-x uses (F-012, #148) — into ruse `Command`s applied to the core. One fixture, one vocabulary, both
//! editors. Point/mark are 0-based CHARACTER offsets in the corpus; ruse addresses bytes, so a fixture's
//! point is converted to a byte offset going in and ruse's point/mark back to characters coming out (the
//! case-conversion fixtures of #912 are multibyte).
//!
//! The corpus is `tests/parity/emacs/fixtures/corpus.yaml` — JSON-in-YAML (a subset of YAML 1.2), captured
//! by `tools/parity/emacs_oracle.py`. Run with `-- --nocapture` to see the tally.

use ruse_core::{apply_command, CaretGravity, EditorState};
use ruse_tui::headless::Headless;
use ruse_tui::input::emacs_command_by_name;
use serde_json::Value;

mod parity_ratchet;

/// Drive ruse: home the cursor to the fixture's start, then resolve each Emacs command name through the M-x
/// registry and apply it to the core. Mirrors the M-x execution path in `input::feed_cmdline`. Returns the
/// settled state plus any op names the registry did not know (a coverage gap, surfaced not silently eaten).
fn drive_ruse(text: &str, ops: &[String], point: usize) -> (EditorState, Vec<String>) {
    let mut st = EditorState::new(text.as_bytes().to_vec());
    // The Emacs profile rests point BETWEEN characters (D-050 / RFC-0015), the same gravity the frontend
    // installs for `RUSE_PROFILE=emacs`; without it every Emacs edit would be Vim-clamped one column short.
    st.set_caret_gravity(CaretGravity::BetweenChar);
    st.set_cursor(char_to_byte(text, point));
    let mut unknown = Vec::new();
    for op in ops {
        match by_name(op) {
            Some((cmd, n)) => {
                for _ in 0..n {
                    apply_command(&mut st, &cmd);
                }
            }
            None => unknown.push(op.clone()),
        }
    }
    (st, unknown)
}

/// The byte offset of the `n`th character of `text` (its length past the end).
fn char_to_byte(text: &str, n: usize) -> usize {
    text.char_indices().nth(n).map_or(text.len(), |(b, _)| b)
}

/// The number of characters before byte offset `b` of `text`.
fn byte_to_char(text: &str, b: usize) -> i64 {
    text.get(..b).map_or(b, |s| s.chars().count()) as i64
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
        let joined = r_text.join("\n");
        let r_point = byte_to_char(&joined, st.cursor());
        let r_mark = st.mark().map(|m| byte_to_char(&joined, m));
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
        println!("divergent: {}", divergent_names.join(", "));
    }
    if !all_unknown.is_empty() {
        println!(
            "registry gaps (command names M-x cannot resolve yet): {}",
            all_unknown.join(", ")
        );
    }

    let names: Vec<&str> = fixtures
        .iter()
        .map(|fx| fx["name"].as_str().expect("name"))
        .collect();
    parity_ratchet::enforce(
        concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../tests/parity/emacs/known-divergences.txt"
        ),
        &names,
        &divergent_names,
    );
}

/// A corpus op (`tools/parity/emacs_oracle.py`): `[C-u [N] ]COMMAND`, with ` CHAR` after
/// `self-insert-command` (#1156). `prefix` is the prefix argument's value (a bare `C-u` is 4).
struct Op<'a> {
    prefix: Option<(u32, Option<&'a str>)>,
    name: &'a str,
    char: Option<char>,
}

fn parse_op(op: &str) -> Op<'_> {
    let mut rest = op;
    let mut prefix = None;
    if let Some(after) = rest.strip_prefix("C-u ") {
        rest = after;
        prefix = Some((4, None));
        if let Some((digits, tail)) = after.split_once(' ') {
            if let Ok(n) = digits.parse::<u32>() {
                prefix = Some((n, Some(digits)));
                rest = tail;
            }
        }
    }
    match rest.strip_prefix("self-insert-command ") {
        Some(c) => Op {
            prefix,
            name: "self-insert-command",
            char: c.chars().next(),
        },
        None => Op {
            prefix,
            name: rest,
            char: None,
        },
    }
}

/// The by-name path's command for an op, and how many times to apply it: it has no prefix argument, so a
/// prefix of N runs the command N times, which is what it means for the fixtured commands (self-insert and
/// the character motions).
fn by_name(op: &str) -> Option<(ruse_core::Command, u32)> {
    let op = parse_op(op);
    let cmd = match op.char {
        Some(c) => ruse_core::Command::InsertChar(c),
        None => emacs_command_by_name(op.name)?,
    };
    Some((cmd, op.prefix.map_or(1, |(n, _)| n)))
}

/// The keys of an op: the prefix argument (`C-u`, then its digits), then the command's keys, or the char a
/// `self-insert-command` inserts.
fn op_keys(op: &str) -> String {
    let op = parse_op(op);
    let mut keys = String::new();
    if let Some((_, digits)) = op.prefix {
        keys.push_str("<C-u>");
        keys.push_str(digits.unwrap_or(""));
    }
    match op.char {
        Some(c) => keys.push(c),
        None => keys.push_str(&emacs_keys(op.name)),
    }
    keys
}

/// The keys that run an Emacs command in Emacs 31.1's default `global-map`, in the headless harness's Vim
/// notation (`<C-f>`, `<M-f>`, …; `<M-x>` is `ESC x`, as a terminal sends it). A command with no default
/// binding that a legacy terminal can send (`capitalize-region`; `kill-whole-line` is `C-S-<backspace>`) is
/// typed as `M-x {name} RET`, which is still the real key path.
fn emacs_keys(op: &str) -> String {
    let bound = match op {
        "back-to-indentation" => "<M-m>",
        "backward-char" => "<C-b>",
        "backward-kill-word" => "<M-BS>",
        "backward-paragraph" => "<M-{>",
        "backward-word" => "<M-b>",
        "beginning-of-buffer" => "<M-<>",
        "capitalize-word" => "<M-c>",
        "delete-char" => "<C-d>",
        "delete-horizontal-space" => "<M-\\>",
        "delete-indentation" => "<M-^>",
        "downcase-region" => "<C-x><C-l>",
        "downcase-word" => "<M-l>",
        "end-of-buffer" => "<M->>",
        "exchange-point-and-mark" => "<C-x><C-x>",
        "forward-char" => "<C-f>",
        "forward-paragraph" => "<M-}>",
        "forward-word" => "<M-f>",
        "just-one-space" => "<M-Space>",
        "kill-line" => "<C-k>",
        "kill-region" => "<C-w>",
        "kill-ring-save" => "<M-w>",
        "kill-word" => "<M-d>",
        "mark-word" => "<M-@>",
        "move-beginning-of-line" => "<C-a>",
        "move-end-of-line" => "<C-e>",
        "newline" => "<CR>",
        "next-line" => "<C-n>",
        "open-line" => "<C-o>",
        "previous-line" => "<C-p>",
        "set-mark-command" => "<C-Space>",
        "transpose-chars" => "<C-t>",
        "transpose-words" => "<M-t>",
        "upcase-region" => "<C-x><C-u>",
        "upcase-word" => "<M-u>",
        "yank" => "<C-y>",
        _ => "",
    };
    if bound.is_empty() {
        format!("<M-x>{op}<CR>")
    } else {
        bound.to_string()
    }
}

/// Fixtures of the Emacs corpus that diverge when their commands are TYPED (#895) rather than resolved by
/// name. The by-name comparator above skips the keymap and the session; this list is what the real key
/// path costs today. Same ratchet: a new divergence or a stale entry fails the test.
const KEY_PATH_DIVERGENCES: &str =
    include_str!("../../../tests/parity/emacs/known-key-divergences.txt");

/// The Emacs corpus through the REAL key path (#895, #604): each command name becomes the keys Emacs binds
/// it to, typed into a headless Emacs-profile session, so the keymap and `Session::on_key` (the session's
/// intercepts included) decide what runs. Observables and oracle are the by-name comparator's.
#[test]
fn parity_emacs_corpus_through_real_keys() {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/parity/emacs/fixtures/corpus.yaml"
    );
    let raw = std::fs::read_to_string(path).expect("corpus");
    let corpus: Value = serde_json::from_str(&raw).expect("corpus is JSON-in-YAML");
    let fixtures = corpus["fixtures"].as_array().expect("fixtures array");

    let mut divergent: Vec<String> = Vec::new();
    for fx in fixtures {
        let name = fx["name"].as_str().expect("name");
        let ops = as_op_vec(&fx["ops"]);
        let point = fx.get("point").and_then(Value::as_u64).unwrap_or(0) as usize;
        let expect = &fx["expect"];

        let text = fx["text"].as_str().expect("text");
        let mut h = Headless::emacs(text);
        h.set_cursor(char_to_byte(text, point));
        let keys: String = ops.iter().map(|op| op_keys(op)).collect();
        h.keys(&keys);

        let (kill, _) = h.register('"');
        let joined = h.text();
        let r_text: Vec<String> = joined.split('\n').map(str::to_string).collect();
        let r_point = byte_to_char(&joined, h.cursor());
        let r_mark = h.mark().map(|m| byte_to_char(&joined, m));
        let r_kill = (!kill.is_empty()).then_some(kill);

        let mut diffs = Vec::new();
        if r_text != as_str_vec(&expect["text"]) {
            diffs.push(format!(
                "text {r_text:?} != {:?}",
                as_str_vec(&expect["text"])
            ));
        }
        if r_point != expect["point"].as_i64().expect("point") {
            diffs.push(format!("point {r_point} != {}", expect["point"]));
        }
        if r_mark != opt_i64(&expect["mark"]) {
            diffs.push(format!("mark {r_mark:?} != {:?}", opt_i64(&expect["mark"])));
        }
        if r_kill != opt_str(&expect["kill"]) {
            diffs.push(format!("kill {r_kill:?} != {:?}", opt_str(&expect["kill"])));
        }
        if diffs.is_empty() {
            println!("  VERIFIED  {name:<28} keys={keys:?}");
        } else {
            println!("  DIVERGENT {name:<28} keys={keys:?}");
            for d in &diffs {
                println!("              - {d}");
            }
            divergent.push(name.to_string());
        }
    }
    println!(
        "\n=== emacs keys: {}/{} verified ===",
        fixtures.len() - divergent.len(),
        fixtures.len()
    );

    let names: Vec<&str> = fixtures
        .iter()
        .map(|fx| fx["name"].as_str().expect("name"))
        .collect();
    let allowed = parity_ratchet::parse(KEY_PATH_DIVERGENCES).expect("the key-path list parses");
    if let Err(e) = parity_ratchet::check(&allowed, &names, &divergent) {
        panic!("emacs key-path ratchet (tests/parity/emacs/known-key-divergences.txt):\n{e}");
    }
}
