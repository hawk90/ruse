//! Reusing the last search for an empty pattern. Split from `ex.rs` (#1385).

use super::*;

/// An empty pattern with no last search to borrow ([`reuse_last_search`]). `Display` is the status message
/// Vim shows: `E476` for `:s` and `:g` (`do_sub` and `ex_global` report it after `E35`), `E35` for `:sort`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum NoPreviousPattern {
    /// `:s//repl/`.
    Substitute,
    /// `:g//` / `:v//`.
    Global,
    /// `:sort //`.
    Sort,
}

impl std::fmt::Display for NoPreviousPattern {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            NoPreviousPattern::Substitute => {
                "E476: Invalid command: E35: No previous regular expression"
            }
            NoPreviousPattern::Global => "E476: Invalid command",
            NoPreviousPattern::Sort => "E35: No previous regular expression",
        })
    }
}

/// Fill an empty pattern from the last search pattern, as Vim does for every Ex command that takes one
/// (#804): `:s//repl/`, `:g//cmd` / `:v//cmd`, `:sort //`, and the `:s//` inside `:g/pat/s//repl/`, which
/// gets the `:g` pattern (Vim's `:g` makes its pattern the last search before it runs the command). A
/// no-op for a non-empty pattern or another command.
///
/// # Errors
/// The status message when an empty pattern has nothing to borrow: `E476` for `:s` and `:g` (Vim's `ex_global`
/// reports it after `E35`), `E35` for `:sort`.
pub(crate) fn reuse_last_search(
    ex: &mut Ex,
    last_search: Option<&str>,
) -> Result<(), NoPreviousPattern> {
    match ex {
        Ex::Substitute(spec) => {
            if spec.pattern.is_empty() {
                spec.pattern = last_search
                    .ok_or(NoPreviousPattern::Substitute)?
                    .to_string();
            }
        }
        Ex::Global(spec) => {
            if spec.pattern.is_empty() {
                spec.pattern = last_search.ok_or(NoPreviousPattern::Global)?.to_string();
            }
            if let GlobalPayload::Core(GlobalCmd::Substitute { pattern, .. }) = &mut spec.cmd {
                if pattern.is_empty() {
                    pattern.clone_from(&spec.pattern);
                }
            }
        }
        Ex::Sort(_, opts) if opts.pattern.as_deref() == Some("") => {
            opts.pattern = Some(last_search.ok_or(NoPreviousPattern::Sort)?.to_string());
        }
        _ => {}
    }
    Ok(())
}
