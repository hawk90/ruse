//! Differential test of the Vim-pattern front-end against the pinned Neovim (issue #692).
//!
//! Every row of `pattern_oracle/cases.rs` was captured from nvim by `tools/parity/pattern_oracle.py`
//! (first match of the pattern in the subject: `matchstrpos` for one line, `searchpos` over a buffer for
//! several lines joined by "\n" or under 'smartcase'). This test replays each row through
//! `Regex::compile` + `find_at(hay, 0)`:
//!
//! - `Expect::Agree`: ruse returns nvim's span, or no match; when nvim reports an error, ruse must
//!   refuse the pattern too (any `RegexError`).
//! - `Expect::Unsupported`: an atom the wrapped engine cannot express — ruse must return
//!   `RegexError::Unsupported`, never a guessed match.
//!
//! All rows are checked before failing, so one run lists every divergence.

use ruse_core::{Regex, RegexError, RegexOptions};

#[path = "pattern_oracle/cases.rs"]
mod cases;

/// What nvim answered.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Nvim {
    Match(usize, usize),
    NoMatch,
    Error,
}

/// What ruse must do with the row.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Expect {
    Agree,
    Unsupported,
}

#[derive(Debug)]
pub(crate) struct Case {
    pub pat: &'static str,
    pub hay: &'static str,
    pub ic: bool,
    pub scs: bool,
    pub nvim: Nvim,
    pub expect: Expect,
}

fn check(case: &Case) -> Result<(), String> {
    let opts = RegexOptions {
        ignore_case: case.ic,
        smart_case: case.scs,
        ..RegexOptions::default()
    };
    let compiled = Regex::compile(case.pat, opts);
    let got = compiled
        .as_ref()
        .map(|re| re.find_at(case.hay, 0).map(|m| (m.start, m.end)));
    match (case.expect, case.nvim, &got) {
        (Expect::Unsupported, _, Err(RegexError::Unsupported(_))) => Ok(()),
        (Expect::Unsupported, _, _) => Err(format!("expected Unsupported, got {got:?}")),
        (Expect::Agree, Nvim::Error, Err(_)) => Ok(()),
        (Expect::Agree, Nvim::NoMatch, Ok(None)) => Ok(()),
        (Expect::Agree, Nvim::Match(s, e), Ok(Some(span))) if *span == (s, e) => Ok(()),
        (Expect::Agree, nvim, _) => Err(format!("nvim {nvim:?}, ruse {got:?}")),
    }
}

#[test]
fn vim_patterns_match_like_nvim() {
    let failures: Vec<String> = cases::CASES
        .iter()
        .filter_map(|c| {
            check(c).err().map(|why| {
                format!(
                    "  {:?} on {:?} (ic={}, scs={}): {why}",
                    c.pat, c.hay, c.ic, c.scs
                )
            })
        })
        .collect();
    assert!(
        failures.is_empty(),
        "{} of {} oracle rows diverge:\n{}",
        failures.len(),
        cases::CASES.len(),
        failures.join("\n")
    );
}
