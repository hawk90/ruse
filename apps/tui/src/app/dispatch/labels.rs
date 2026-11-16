//! Status-line message text: buffer labels, the `:ls` line, substitute tallies and regex errors.

use ruse_core::Workspace;

use crate::app::messages::Msg;

/// A short label for the focused buffer (name + `[+]` when modified) — the status after `:b`/`:bn`/`:bp`.
pub(crate) fn focused_buffer_label(ws: &Workspace) -> String {
    let id = ws.focused_buffer();
    let name = ws.buffer_name(id).unwrap_or("[No Name]");
    let dirty = if ws.focused().doc.is_modified() {
        " [+]"
    } else {
        ""
    };
    format!("buffer {}: {name}{dirty}", id.0)
}

/// The one-line `:ls` buffer list: `1%a name  2  # other  3 + scratch …` (`%`=current, `#`=alternate,
/// `+`=modified). Vim renders a multi-line list; the single status line shows a compact form for MVP.
pub(crate) fn buffer_list_line(ws: &Workspace) -> String {
    ws.buffers()
        .iter()
        .map(|b| {
            let cur = if b.current { "%" } else { "" };
            let alt = if b.alt { "#" } else { "" };
            let modified = if b.modified { "+" } else { "" };
            format!("{}{cur}{alt} {}{modified}", b.id.0, b.name)
        })
        .collect::<Vec<_>>()
        .join("  ")
}

/// The `:s///n` report-only status line. Wording + per-noun pluralization verified vs nvim v0.12.4:
/// `6 matches on 3 lines`, `2 matches on 1 line`, `1 match on 1 line` (each noun pluralizes on its own
/// count). The no-match case is reported separately (E486) by the caller, never here.
pub(crate) fn match_count_message(matches: usize, lines: usize) -> String {
    let m = if matches == 1 { "match" } else { "matches" };
    let l = if lines == 1 { "line" } else { "lines" };
    format!("{matches} {m} on {lines} {l}")
}

/// The status line after a substitute replaced text: `N substitutions on M lines` (one wording for `:s`,
/// `&`, `g&`, `:&&` and the `:s///c` loop), when there were more substitutions than `report`; else nothing.
pub(crate) fn substituted_message(out: &ruse_core::SubOutcome, report: i64) -> Msg {
    crate::app::report::substituted(out.replacements, out.lines, report)
        .map_or_else(|| Msg::echo(""), Msg::info)
}

/// Status for a regex compile error (F-009): the pattern reader's own `E…:` message when it has one (nvim's
/// number and wording, #1087), else a plain note. When nvim gives a second error after the first (the NFA
/// engine's `E870` after a bad `\{…}`), that one leads, since it is the last error nvim reports.
pub(crate) fn regex_error_msg(e: &ruse_core::RegexError) -> String {
    match e {
        ruse_core::RegexError::Unsupported(m) => format!("E: unsupported pattern: {m}"),
        ruse_core::RegexError::Syntax(m) if m.starts_with("E554:") => {
            format!("E870: (NFA regexp) Error while reading repetition limits: {m}")
        }
        ruse_core::RegexError::Syntax(m) if has_error_number(m) => m.clone(),
        ruse_core::RegexError::Syntax(m) => format!("E: bad pattern: {m}"),
    }
}

/// The status for an `:s` whose pattern cannot be used: nvim follows the pattern's own error with `E476`
/// (`do_sub`), and so does `:g` (`ex_global`).
pub(crate) fn invalid_command_msg(e: &ruse_core::RegexError) -> String {
    format!("E476: Invalid command: {}", regex_error_msg(e))
}

/// Whether `msg` opens with a Vim error number (`E54: …`).
fn has_error_number(msg: &str) -> bool {
    let digits = msg
        .strip_prefix('E')
        .map_or(0, |r| r.bytes().take_while(u8::is_ascii_digit).count());
    digits > 0 && msg[1 + digits..].starts_with(':')
}
