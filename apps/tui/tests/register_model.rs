//! Register invariants against a shadow model of Vim's register rules (#906, `:help registers`).
//!
//! Random yanks, deletes, puts, motions and undos run through the real key path (the headless session,
//! #895) while a model written from `:help registers` and nvim's `op_delete` / `op_yank` keeps its own
//! copy of every register. After each key sequence the unnamed register, `"0`, the delete ring `"1`–`"9`,
//! the small-delete register `"-` and `"a`/`"b` must match it, text and kind. The model is not the store's
//! code: it computes the text an operator takes from the line and the cursor, and routes it by the rules:
//!
//! - a yank with no register writes `"0` and the unnamed register; a named one writes that register
//!   (uppercase appends) and the unnamed register mirrors it; `"0` is left alone;
//! - a delete names its register first; then a linewise or multi-line delete shifts `"1`→…→`"9` and
//!   writes `"1` whatever register was named; then a delete within one line with no register named writes
//!   `"-`; the unnamed register follows the last of those, except an uppercase append keeps the whole
//!   appended register;
//! - `"_` takes nothing; puts and undo write no register.
//!
//! Lines are ASCII words, so one byte is one character; a charwise op the cursor's line cannot take (an
//! empty line, `X` in column 0, `yj` on the last line) is not generated. Each case is checked against the
//! model only, but the model itself was checked against nvim v0.12.6 on a fixed sample (PR #906 notes).

use proptest::prelude::*;
use ruse_core::{RegKind, Register};
use ruse_tui::headless::Headless;

/// The register a key sequence names: none, a letter (lower writes, upper appends) or the black hole.
#[derive(Clone, Copy, Debug)]
enum Name {
    None,
    A,
    UpperA,
    B,
    Hole,
}

impl Name {
    fn prefix(self) -> &'static str {
        match self {
            Name::None => "",
            Name::A => "\"a",
            Name::UpperA => "\"A",
            Name::B => "\"b",
            Name::Hole => "\"_",
        }
    }
}

/// One step: an operator over a fixed span, a put, a motion or undo.
#[derive(Clone, Copy, Debug)]
enum Step {
    Yank(Name, &'static str),
    Delete(Name, &'static str),
    Put(&'static str),
    Move(&'static str),
    Undo,
}

fn step() -> impl Strategy<Value = Step> {
    let name = prop_oneof![
        3 => Just(Name::None),
        1 => Just(Name::A),
        1 => Just(Name::UpperA),
        1 => Just(Name::B),
        1 => Just(Name::Hole),
    ];
    prop_oneof![
        3 => (name.clone(), prop::sample::select(vec!["yy", "yj", "yl", "y$"]))
            .prop_map(|(n, k)| Step::Yank(n, k)),
        4 => (name, prop::sample::select(vec!["dd", "dj", "x", "X", "D", "d$"]))
            .prop_map(|(n, k)| Step::Delete(n, k)),
        2 => prop::sample::select(vec!["p", "P", "\"ap", "\"1p", "\"2P", "\"-p"]).prop_map(Step::Put),
        3 => prop::sample::select(vec!["j", "k", "w", "0", "$", "gg", "G", "l"]).prop_map(Step::Move),
        1 => Just(Step::Undo),
    ]
}

/// A register as the model holds it: text and whether it is linewise (unset = empty charwise).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Reg {
    text: Vec<u8>,
    linewise: bool,
}

impl Reg {
    fn of(r: &Register) -> Reg {
        Reg {
            text: r.text().to_vec(),
            linewise: r.kind() == RegKind::Linewise,
        }
    }

    /// `"A`-style append: a linewise side makes the result linewise (whole lines, so a charwise side gets
    /// its own line).
    fn append(&self, add: &Reg) -> Reg {
        if self.text.is_empty() {
            return add.clone();
        }
        let mut text = self.text.clone();
        if add.linewise && !self.linewise {
            text.push(b'\n');
        }
        text.extend_from_slice(&add.text);
        let linewise = self.linewise || add.linewise;
        // A linewise register holds whole lines: charwise text appended to one is a line of its own.
        if linewise && text.last() != Some(&b'\n') {
            text.push(b'\n');
        }
        Reg { text, linewise }
    }
}

#[derive(Default)]
struct Model {
    unnamed: Reg,
    zero: Reg,
    ring: [Reg; 9],
    small: Reg,
    a: Reg,
    b: Reg,
}

impl Model {
    /// Write a named register (or append to it); returns what the unnamed register then holds.
    fn write_named(&mut self, name: Name, r: &Reg) -> Reg {
        match name {
            Name::A => {
                self.a = r.clone();
                self.a.clone()
            }
            Name::UpperA => {
                self.a = self.a.append(r);
                self.a.clone()
            }
            Name::B => {
                self.b = r.clone();
                self.b.clone()
            }
            Name::None | Name::Hole => unreachable!("not a named register"),
        }
    }

    fn yank(&mut self, name: Name, r: Reg) {
        match name {
            Name::Hole => {}
            Name::None => {
                self.zero = r.clone();
                self.unnamed = r;
            }
            n => self.unnamed = self.write_named(n, &r),
        }
    }

    fn delete(&mut self, name: Name, r: Reg) {
        if let Name::Hole = name {
            return;
        }
        let multiline = r.linewise || r.text.contains(&b'\n');
        if !matches!(name, Name::None) {
            self.unnamed = self.write_named(name, &r);
        }
        if multiline {
            self.ring.rotate_right(1);
            self.ring[0] = r.clone();
            if !matches!(name, Name::UpperA) {
                self.unnamed = r.clone();
            }
        }
        if matches!(name, Name::None) {
            if !multiline {
                self.small = r.clone();
            }
            self.unnamed = r;
        }
    }
}

/// The lines of `text` (a final newline ends the last line rather than starting another).
fn lines(text: &str) -> Vec<&str> {
    let body = text.strip_suffix('\n').unwrap_or(text);
    body.split('\n').collect()
}

/// What `keys` takes from the buffer at (`row`, `col`), or `None` when the op cannot run there. A cursor
/// past the last line takes nothing: a buffer whose unterminated last line was emptied (`"ap` into an
/// emptied buffer, then `x`) reads one line short and leaves the cursor on the slot after it (#730 family,
/// reported with #906), which is no register question.
///
/// An empty buffer is nvim's "--No lines in buffer--" (every line deleted): `yy` yanks an empty line
/// there, `dd` fails.
fn span(keys: &str, text: &str, row: usize, col: usize) -> Option<Reg> {
    let ls = lines(text);
    let line = ls.get(row)?;
    let line_reg = |n: usize| Reg {
        text: ls[row..row + n]
            .iter()
            .flat_map(|l| l.bytes().chain(std::iter::once(b'\n')))
            .collect(),
        linewise: true,
    };
    let chars = |s: &str| Reg {
        text: s.as_bytes().to_vec(),
        linewise: false,
    };
    match keys {
        "dd" if text.is_empty() => None,
        "yy" | "dd" => Some(line_reg(1)),
        "yj" | "dj" => (row + 1 < ls.len()).then(|| line_reg(2)),
        _ if line.is_empty() => None,
        "yl" | "x" => Some(chars(&line[col..=col])),
        "X" => (col > 0).then(|| chars(&line[col - 1..col])),
        "y$" | "D" | "d$" => Some(chars(&line[col..])),
        other => unreachable!("no span for {other}"),
    }
}

/// Compare every register the model keeps with the store.
fn check(h: &Headless, m: &Model, after: &[String]) -> Result<(), TestCaseError> {
    let r = h.registers();
    let pairs: Vec<(&str, Reg, &Reg)> = vec![
        ("\"", Reg::of(r.unnamed()), &m.unnamed),
        ("0", Reg::of(r.yank0()), &m.zero),
        ("-", Reg::of(r.small_delete()), &m.small),
        ("a", Reg::of(r.get(Some('a'))), &m.a),
        ("b", Reg::of(r.get(Some('b'))), &m.b),
    ];
    for (name, got, want) in pairs {
        prop_assert_eq!(&got, want, "register {} after {:?}", name, after);
    }
    for n in 1..=9 {
        let got = Reg::of(r.numbered(n).expect("1..=9"));
        prop_assert_eq!(&got, &m.ring[n - 1], "register {} after {:?}", n, after);
    }
    Ok(())
}

/// `n` cases, scaled by `RUSE_PROPTEST_SCALE` for the scheduled deep tier (#920).
fn cases(n: u32) -> ProptestConfig {
    let scale = std::env::var("RUSE_PROPTEST_SCALE")
        .ok()
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(1)
        .max(1);
    ProptestConfig {
        cases: n.saturating_mul(scale),
        ..ProptestConfig::default()
    }
}

proptest! {
    #![proptest_config(cases(64))]

    #[test]
    fn registers_follow_the_vim_rules(
        init in prop::collection::vec("[a-c]{1,3}( [a-c]{1,3}){0,2}", 2..5),
        steps in prop::collection::vec(step(), 1..24),
    ) {
        let mut h = Headless::vim(&(init.join("\n") + "\n"));
        let mut model = Model::default();
        let mut done: Vec<String> = Vec::new();
        for s in steps {
            let text = h.text();
            let (row, col) = h.cursor_row_col();
            let row = row - 1;
            let keys = match s {
                Step::Yank(n, k) | Step::Delete(n, k) => {
                    let Some(r) = span(k, &text, row, col) else { continue };
                    if let Step::Yank(..) = s {
                        model.yank(n, r);
                    } else {
                        model.delete(n, r);
                    }
                    format!("{}{k}", n.prefix())
                }
                Step::Put(k) | Step::Move(k) => k.to_string(),
                Step::Undo => "u".to_string(),
            };
            h.keys(&keys);
            done.push(keys);
            check(&h, &model, &done)?;
            h.assert_invariants();
        }
    }
}
