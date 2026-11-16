//! Showing option values: `:set opt?` and `:set all`. Split from `set_cmd.rs` (#1388).

use super::*;

/// How `:set opt?` shows a value: `  name=value` for a number or string, `  name` / `noname` for a
/// boolean (nvim's `showoneopt`).
#[must_use]
pub fn show_one(def: &OptDef, val: OptVal) -> String {
    match val {
        OptVal::Bool(true) => format!("  {}", def.name),
        OptVal::Bool(false) => format!("no{}", def.name),
        OptVal::Num(n) => format!("  {}={n}", def.name),
        OptVal::Str(s) => format!("  {}={s}", def.name),
        OptVal::Text(s) => format!("  {}={s}", def.name),
    }
}

/// The rows of an option listing (`:set`, `:set all`) on a `cols`-wide screen, the way nvim's
/// `showoptions` lays them out: a title row, then the short entries in columns 20 cells apart, filled
/// column by column, then every entry too long for a column on a row of its own.
#[must_use]
pub fn listing(title: &str, items: &[(&OptDef, OptVal)], cols: usize) -> Vec<String> {
    const INC: usize = 20;
    const GAP: usize = 3;
    let mut rows = vec![title.to_string()];
    let fits = |d: &OptDef, v: &OptVal| match v {
        OptVal::Bool(_) => true,
        _ => show_one(d, v.clone()).len() - 2 <= INC - GAP,
    };
    let short: Vec<String> = items
        .iter()
        .filter(|(d, v)| fits(d, v))
        .map(|(d, v)| show_one(d, v.clone()))
        .collect();
    if !short.is_empty() {
        let ncols = ((cols + GAP).saturating_sub(3) / INC).max(1);
        let nrows = short.len().div_ceil(ncols);
        for r in 0..nrows {
            let mut line = String::new();
            for (k, item) in short.iter().enumerate().skip(r).step_by(nrows) {
                let col = (k / nrows) * INC;
                while line.len() < col {
                    line.push(' ');
                }
                line.push_str(item);
            }
            rows.push(line);
        }
    }
    rows.extend(
        items
            .iter()
            .filter(|(d, v)| !fits(d, v))
            .map(|(d, v)| show_one(d, v.clone())),
    );
    rows
}
