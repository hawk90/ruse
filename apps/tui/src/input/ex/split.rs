//! `|` command separation. Split from `ex.rs` (#1385).

/// Split a typed `:`-line into the commands its `|` separators chain (`:help :bar`), in order. Each piece
/// has its leading whitespace and `:` stripped. As in Vim:
/// - `:g`/`:v`, `:normal`, and the `!` shell forms (`:!`, `:{range}!`, `:r !`) see the `|` as part of their
///   argument, so the rest of the line is theirs;
/// - `:s/pat/rep/` splits only after its flags: a `|` inside the pattern or replacement is literal;
/// - `\|` never splits. Unlike Vim the backslash is KEPT, so a regex alternation in a `:sort /a\|b/`
///   pattern survives (Vim drops it for commands that do not parse their own `|`).
///
/// A line without a separator comes back as one piece, unchanged.
#[must_use]
pub fn split_bar(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = line;
    loop {
        let cmd = rest.trim_start_matches(|c: char| c.is_whitespace() || c == ':');
        let (piece, next) = split_first_command(cmd);
        out.push(piece.to_string());
        match next {
            Some(n) => rest = n,
            None => return out,
        }
    }
}

/// One step of [`split_bar`]: the first command of `cmd` and the text after its separating `|`, if any.
fn split_first_command(cmd: &str) -> (&str, Option<&str>) {
    // Skip the range prefix (line numbers, `.`/`$`/`%`, `,`/`;`, relative `+`/`-` offsets, `'x` marks).
    let bytes = cmd.as_bytes();
    // Command modifiers (`:silent normal x|y`) come before the range; the command after them decides.
    let mut i = cmd.len() - super::ex_cmd::strip_modifiers(cmd).1.len();
    while i < bytes.len() {
        match bytes[i] {
            b'0'..=b'9' | b',' | b';' | b'%' | b'*' | b'.' | b'$' | b'+' | b'-' | b' ' | b'\t' => {
                i += 1
            }
            b'\'' | b'\\' if i + 1 < bytes.len() => {
                i += 1;
                i += cmd[i..].chars().next().map_or(0, char::len_utf8);
            }
            // A pattern address (`:/a|b/d`): its `|` is part of the pattern.
            delim @ (b'/' | b'?') => {
                i = skip_unescaped(cmd, i + 1, delim as char, 1).unwrap_or(bytes.len());
            }
            _ => break,
        }
    }
    let after_range = &cmd[i..];
    let verb_len = after_range
        .find(|c: char| !c.is_ascii_alphabetic())
        .unwrap_or(after_range.len());
    let verb = &after_range[..verb_len];
    let arg = &after_range[verb_len..];
    let whole = || (cmd, None);
    // Commands whose argument runs to the end of the line, `|` included.
    let is_prefix = |full: &str, min: usize| verb.len() >= min && full.starts_with(verb);
    if verb.is_empty() && arg.starts_with('!')
        || is_prefix("global", 1)
        || is_prefix("vglobal", 1)
        || is_prefix("normal", 4)
        || is_prefix("folddoopen", 5)
        || is_prefix("folddoclosed", 7)
        || (is_prefix("read", 1) && arg.trim_start().starts_with('!'))
        // `:w !cmd` / `:up !cmd` pipe to a shell command (the `!` after a blank; `:w!` is the bang).
        || ((is_prefix("write", 1) || is_prefix("update", 2))
            && arg.starts_with(char::is_whitespace)
            && arg.trim_start().starts_with('!'))
    {
        return whole();
    }
    // `:s/pat/rep/flags` — scan past the pattern and the replacement first.
    let mut scan_from = i;
    if is_prefix("substitute", 1) {
        if let Some(delim) = arg.chars().next() {
            if delim.is_ascii_punctuation() && !matches!(delim, '\\' | '"' | '|') {
                let body_start = i + verb_len + delim.len_utf8();
                let pend =
                    body_start + ruse_core::pattern::skip_regexp(&cmd[body_start..], delim, true);
                let rep_start = cmd[pend..]
                    .starts_with(delim)
                    .then(|| pend + delim.len_utf8());
                match rep_start.and_then(|r| skip_unescaped(cmd, r, delim, 1)) {
                    Some(end) => scan_from = end,
                    None => return whole(), // an unterminated pattern/replacement owns the `|`
                }
            }
        }
    }
    let mut chars = cmd[scan_from..].char_indices();
    while let Some((j, c)) = chars.next() {
        match c {
            '\\' => {
                chars.next(); // an escaped char (`\|` included) never separates
            }
            '|' => {
                let at = scan_from + j;
                return (&cmd[..at], Some(&cmd[at + 1..]));
            }
            _ => {}
        }
    }
    (cmd, None)
}

/// The byte offset just past the `n`-th unescaped `delim` in `s` from `from` (a `\` escapes the next char),
/// or `None` when fewer than `n` delimiters follow.
pub(super) fn skip_unescaped(s: &str, from: usize, delim: char, n: usize) -> Option<usize> {
    let mut seen = 0;
    let mut chars = s[from..].char_indices();
    while let Some((j, c)) = chars.next() {
        if c == '\\' {
            chars.next();
        } else if c == delim {
            seen += 1;
            if seen == n {
                return Some(from + j + c.len_utf8());
            }
        }
    }
    None
}
