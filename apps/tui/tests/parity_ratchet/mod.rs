//! Known-divergence ratchet shared by the parity comparators (#883).
//!
//! A comparator replays every oracle fixture through ruse and sorts it into verified or divergent. The
//! committed list `tests/parity/<editor>/known-divergences.txt` names the fixtures that are ALLOWED to
//! diverge today. The comparator fails when:
//!
//! - a fixture diverges that is not on the list (a regression, or a new fixture ruse does not match yet;
//!   fix ruse, or list the fixture with a reason),
//! - a listed fixture now verifies (stale entry; delete the line so the gain is locked in),
//! - a listed name is no fixture in the corpus, or is listed twice.
//!
//! So the list can only shrink unless someone edits it in review. File format: one fixture name per line;
//! `#` starts a comment; blank lines are ignored.

use std::collections::BTreeSet;

/// Parse a known-divergences file body into fixture names, rejecting duplicates.
pub fn parse(body: &str) -> Result<Vec<String>, String> {
    let mut seen = BTreeSet::new();
    let mut names = Vec::new();
    for (i, line) in body.lines().enumerate() {
        let name = line.split('#').next().unwrap_or("").trim();
        if name.is_empty() {
            continue;
        }
        if !seen.insert(name.to_string()) {
            return Err(format!("line {}: `{name}` is listed twice", i + 1));
        }
        names.push(name.to_string());
    }
    Ok(names)
}

/// Check one comparator run against the allowed list. `fixtures` is every fixture name in the corpus,
/// `divergent` the ones that diverged in this run. Returns every violation, one per line.
pub fn check(allowed: &[String], fixtures: &[&str], divergent: &[String]) -> Result<(), String> {
    let all: BTreeSet<&str> = fixtures.iter().copied().collect();
    let allowed_set: BTreeSet<&str> = allowed.iter().map(String::as_str).collect();
    let divergent_set: BTreeSet<&str> = divergent.iter().map(String::as_str).collect();

    let mut errors = Vec::new();
    for name in &divergent_set {
        if !allowed_set.contains(name) {
            errors.push(format!(
                "NEW DIVERGENCE `{name}`: ruse no longer matches the oracle (fix it, or list it with a reason)"
            ));
        }
    }
    for name in &allowed_set {
        if !all.contains(name) {
            errors.push(format!(
                "UNKNOWN ENTRY `{name}`: no fixture by that name in the corpus"
            ));
        } else if !divergent_set.contains(name) {
            errors.push(format!(
                "STALE ENTRY `{name}`: the fixture now verifies; delete its line to lock in the gain"
            ));
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("\n"))
    }
}

/// Read the list at `path` and check the run, panicking with every violation.
pub fn enforce(path: &str, fixtures: &[&str], divergent: &[String]) {
    let body = std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("cannot read known-divergence list {path}: {e}"));
    let allowed = parse(&body).unwrap_or_else(|e| panic!("{path}: {e}"));
    if let Err(e) = check(&allowed, fixtures, divergent) {
        panic!("parity ratchet failed against {path}:\n{e}");
    }
}
