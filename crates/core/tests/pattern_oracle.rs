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
//! - `Expect::Known(n)`: a tracked divergence (issue #n) the wrapped engine cannot close yet. ruse must
//!   still DISAGREE with nvim: a row that starts agreeing fails, so it is promoted to `Agree` (the same
//!   ratchet as `tests/parity/vim/known-divergences.txt`).
//!
//! All rows are checked before failing, so one run lists every divergence. Rows are grouped by class
//! (#912); `-- --nocapture` prints the per-class tally.

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
    Known(u32),
}

#[derive(Debug)]
pub(crate) struct Case {
    pub class: &'static str,
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
    let got = compiled.as_ref().map(|re| {
        re.find_at(case.hay, ruse_core::BytePos(0))
            .map(|m| (m.start.0, m.end.0))
    });
    match (case.expect, case.nvim, &got) {
        (Expect::Unsupported, _, Err(RegexError::Unsupported(_))) => Ok(()),
        (Expect::Unsupported, _, _) => Err(format!("expected Unsupported, got {got:?}")),
        (Expect::Known(n), nvim, _) if agrees(nvim, &got) => Err(format!(
            "known divergence #{n} now agrees with nvim {nvim:?}: mark the row agreed"
        )),
        (Expect::Known(_), _, _) => Ok(()),
        (Expect::Agree, nvim, _) if agrees(nvim, &got) => Ok(()),
        (Expect::Agree, nvim, _) => Err(format!("nvim {nvim:?}, ruse {got:?}")),
    }
}

type Got<'a> = Result<Option<(usize, usize)>, &'a RegexError>;

/// ruse's answer is nvim's: the same span, no match, or (for a Vim error) any refusal.
fn agrees(nvim: Nvim, got: &Got) -> bool {
    match (nvim, got) {
        (Nvim::Error, Err(_)) | (Nvim::NoMatch, Ok(None)) => true,
        (Nvim::Match(s, e), Ok(Some(span))) => *span == (s, e),
        _ => false,
    }
}

/// The per-class tally (#912): rows, agreed with nvim, rejected as unsupported, known divergences.
#[test]
fn class_table() {
    let mut classes: Vec<(&str, [usize; 3])> = Vec::new();
    for c in cases::CASES {
        let i = match classes.iter().position(|(name, _)| *name == c.class) {
            Some(i) => i,
            None => {
                classes.push((c.class, [0; 3]));
                classes.len() - 1
            }
        };
        let slot = match c.expect {
            Expect::Agree => 0,
            Expect::Unsupported => 1,
            Expect::Known(_) => 2,
        };
        classes[i].1[slot] += 1;
    }
    println!("| class | rows | agree | unsupported | known |");
    println!("|---|---:|---:|---:|---:|");
    let mut total = [0; 3];
    for (name, n) in &classes {
        println!(
            "| {name} | {} | {} | {} | {} |",
            n.iter().sum::<usize>(),
            n[0],
            n[1],
            n[2]
        );
        for k in 0..3 {
            total[k] += n[k];
        }
    }
    println!(
        "| total | {} | {} | {} | {} |",
        total.iter().sum::<usize>(),
        total[0],
        total[1],
        total[2]
    );
    assert_eq!(total.iter().sum::<usize>(), cases::CASES.len());
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
