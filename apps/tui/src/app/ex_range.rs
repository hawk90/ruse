//! The Ex address grammar (`:help :range`, #608): resolve the leading range of a `:` line against the
//! focused buffer into plain line numbers, so the per-command parsers in `input::ex` only ever see `N`,
//! `N,M` or `%`. This is the one place an address is read; every range-taking command goes through it.

use ruse_core::{SubRange, Workspace};

/// An ex line whose leading range [`resolve_ex_range`] turned into plain line numbers.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ExRange {
    /// The line to parse: its range, if any, is now `N` or `N,M` (1-based), or still `%`.
    Line(String),
    /// A range and no command (`:5`, `:$`, `:.+2`, `:/pat/`): go to that line, clamped to the buffer.
    Goto(usize),
    /// The range runs backwards (`:3,2d`): Vim asks `Backwards range given, OK to swap (y/n)?` and, on `y`,
    /// runs this line, which has the two addresses swapped.
    Backwards(String),
}

/// What [`resolve_ex_range`] found: the resolved line or why it was refused; the pattern a `/pat/` or
/// `?pat?` address searched for with its direction, which becomes the last search pattern (`n` repeats
/// it, as in Vim); and the line a `;` moved the cursor to, which Vim keeps even when the command fails.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Resolved {
    pub(crate) range: Result<ExRange, ExRangeError>,
    pub(crate) search: Option<(String, bool)>,
    pub(crate) cursor: Option<usize>,
}

/// The patterns a `\/`, `\?` (the last search) or `\&` (the last substitute) address searches for.
#[derive(Clone, Copy, Default)]
pub(crate) struct AddrPatterns<'a> {
    pub(crate) search: Option<&'a str>,
    pub(crate) substitute: Option<&'a str>,
}

/// Why [`resolve_ex_range`] refused a line. `Display` is nvim's message.
#[derive(Debug, PartialEq, Eq, Clone)]
pub(crate) enum ExRangeError {
    /// An address above the last line or below 0 (`E16`).
    InvalidRange,
    /// A mark address (`'a`, `'<`, `''`, …) whose mark is not set (`E20`).
    MarkNotSet,
    /// `\/`, `\?` or an empty `//` before any search (`E35`).
    NoPreviousPattern,
    /// `\&` before any `:s` (`E33`).
    NoPreviousSubstitute,
    /// A pattern address that matches no line (`E486`).
    PatternNotFound(String),
    /// A pattern address that does not compile (the regex error's own message).
    BadPattern(String),
    /// A zero count after the command (`:d 0`, `E939`).
    PositiveCount,
    /// Text after a command's register and count (`:d x y`, `E488`).
    Trailing(String),
}

impl std::fmt::Display for ExRangeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ExRangeError::InvalidRange => f.write_str("E16: Invalid range"),
            ExRangeError::MarkNotSet => f.write_str("E20: Mark not set"),
            ExRangeError::NoPreviousPattern => f.write_str("E35: No previous regular expression"),
            ExRangeError::NoPreviousSubstitute => {
                f.write_str("E33: No previous substitute regular expression")
            }
            ExRangeError::PatternNotFound(p) => write!(f, "E486: pattern not found: {p}"),
            ExRangeError::BadPattern(m) => f.write_str(m),
            ExRangeError::PositiveCount => f.write_str("E939: Positive count required"),
            ExRangeError::Trailing(t) => write!(f, "E488: Trailing characters: {t}"),
        }
    }
}

/// Resolve the leading range of an ex line (`:help :range`) against the focused buffer, so the numeric
/// range parsers see `N` or `N,M`. An address is one of
/// - a line number, `.` (the cursor's line), `$` (the last line);
/// - `%` (`1,$`) or `*` (`'<,'>`), which stand for two addresses;
/// - a mark: `'a`–`'z`, `'<`/`'>` (the last Visual selection), `'[`/`']` (the last change or yank), `'.`
///   (the last change) and `''` (the line before the latest jump); an unset one, or a file mark, is `E20`;
/// - `/pat/` (the next line that matches, wrapping around the end) or `?pat?` (the previous one), where
///   another `/pat/` or `?pat?` may follow to search on from there (`:/a//b/`), and an empty pattern is the
///   last search; `\/` and `\?` search for the last search pattern, `\&` for the last substitute pattern;
///
/// followed by any `+N`/`-N` offsets; a bare offset counts from the cursor's line. Addresses are separated
/// by `,`, or by `;`, which makes the one before it the current line for the next (`:2;+1`, `:3;/pat/`). A
/// missing address after a separator is the cursor's line. Of more than two addresses the last two count.
///
/// As in Vim, an address above the last line or below 0 is `E16: Invalid range` for every range-taking
/// command (#1079), while a range with no command just goes to the line, clamped. A range that runs
/// backwards comes back as [`ExRange::Backwards`] for the caller to confirm. `:d`, `:y`, `:j`, `:>` and
/// `:<` also take a count after the command (`:d 3`, `:2y a 2`), which becomes the range of that many lines
/// from the range's last line (`:help :d`); the line comes back in the canonical `N,Md x` form. A line
/// without a range comes back unchanged.
pub(crate) fn resolve_ex_range(line: &str, ws: &mut Workspace, pats: AddrPatterns<'_>) -> Resolved {
    let mut side = Side {
        search: None,
        cursor: None,
    };
    let range = resolve(line, ws, pats, &mut side);
    Resolved {
        range,
        search: side.search,
        cursor: side.cursor,
    }
}

/// What resolving a range does besides the range itself (see [`Resolved`]).
struct Side {
    search: Option<(String, bool)>,
    cursor: Option<usize>,
}

fn resolve(
    line: &str,
    ws: &mut Workspace,
    pats: AddrPatterns<'_>,
    side: &mut Side,
) -> Result<ExRange, ExRangeError> {
    let rest = line.trim_start_matches([' ', ':']);
    let (nlines, mut cur_line, visual) = {
        let pane = ws.focused();
        let b = pane.doc.bytes();
        let nlines =
            (b.split(|&c| c == b'\n').count() - usize::from(b.ends_with(b"\n"))).max(1) as i64;
        let cur = (ruse_core::pos::line_of(b, pane.view.cursor()) as i64 + 1).min(nlines);
        let visual = pane.view.last_visual_bounds().map(|(s, e, _)| {
            (
                ruse_core::pos::line_of(b, s) as i64 + 1,
                ruse_core::pos::line_of(b, e) as i64 + 1,
            )
        });
        (nlines, cur, visual)
    };
    let unchanged = || ExRange::Line(rest.to_string());
    let s = rest.as_bytes();
    let digits = |i: &mut usize| -> Option<i64> {
        let start = *i;
        while s.get(*i).is_some_and(u8::is_ascii_digit) {
            *i += 1;
        }
        (start < *i).then(|| rest[start..*i].parse().unwrap_or(i64::MAX))
    };
    // The last search pattern as of this point of the range: a `/pat/` address replaces it for the
    // addresses after it (`:/a/;//d`).
    let mut last_search = pats.search.map(str::to_string);
    let mut whole = false;
    let mut addrs: Vec<i64> = Vec::new();
    let mut i = 0;
    loop {
        while s.get(i) == Some(&b' ') {
            i += 1;
        }
        let start = i;
        let base = match s.get(i) {
            Some(b'0'..=b'9') => digits(&mut i),
            Some(b'.') => {
                i += 1;
                Some(cur_line)
            }
            Some(b'$') => {
                i += 1;
                Some(nlines)
            }
            // `%` is `1,$` and `*` is `'<,'>`: the first address goes in here, the second is the base.
            Some(b'%') => {
                i += 1;
                whole = true;
                addrs.push(1);
                Some(nlines)
            }
            Some(b'*') => {
                let (first, last) = visual.ok_or(ExRangeError::MarkNotSet)?;
                i += 1;
                addrs.push(first);
                Some(last)
            }
            Some(b'\'') => match s.get(i + 1).copied().and_then(|c| mark_line(ws, c, visual)) {
                Some(line) => {
                    i += 2;
                    Some(line.ok_or(ExRangeError::MarkNotSet)?)
                }
                None => return Ok(unchanged()), // not a mark: leave the line to the parser
            },
            Some(b'/' | b'?') => {
                let mut from = cur_line;
                while let Some(&delim @ (b'/' | b'?')) = s.get(i) {
                    let forward = delim == b'/';
                    let (raw, end) = scan_pattern(rest, i + 1, delim as char);
                    i = end;
                    // In `?pat?` a `\?` is the delimiter, not the `?` quantifier.
                    let pat = if forward {
                        raw.to_string()
                    } else {
                        raw.replace("\\?", "?")
                    };
                    let pat = if pat.is_empty() {
                        last_search.clone().ok_or(ExRangeError::NoPreviousPattern)?
                    } else {
                        side.search = Some((pat.clone(), forward));
                        last_search = Some(pat.clone());
                        pat
                    };
                    from = search_line(ws, &pat, from, forward)?;
                }
                Some(from)
            }
            Some(b'\\') => {
                let (pat, forward) = match s.get(i + 1) {
                    Some(b'/') => (last_search.clone(), true),
                    Some(b'?') => (last_search.clone(), false),
                    Some(b'&') => (
                        Some(
                            pats.substitute
                                .ok_or(ExRangeError::NoPreviousSubstitute)?
                                .to_string(),
                        ),
                        true,
                    ),
                    _ => return Ok(unchanged()),
                };
                let pat = pat.ok_or(ExRangeError::NoPreviousPattern)?;
                i += 2;
                Some(search_line(ws, &pat, cur_line, forward)?)
            }
            _ => None,
        };
        let mut v = base.unwrap_or(cur_line);
        let mut have = base.is_some();
        while let Some(&sign @ (b'+' | b'-')) = s.get(i) {
            i += 1;
            let n = digits(&mut i).unwrap_or(1);
            v = if sign == b'+' {
                v.saturating_add(n)
            } else {
                v.saturating_sub(n)
            };
            have = true;
        }
        // No range at all, unless a separator follows: a missing first address is the cursor's line too
        // (`:,d`, `:;+1d`).
        if !have && addrs.is_empty() && i == start && !matches!(s.get(i), Some(b',' | b';')) {
            break;
        }
        addrs.push(v);
        while s.get(i) == Some(&b' ') {
            i += 1;
        }
        match s.get(i) {
            Some(b',') => i += 1,
            Some(b';') => {
                // Vim moves the cursor here (and leaves it there even if the command then fails).
                i += 1;
                cur_line = v.clamp(1, nlines);
                side.cursor = Some(cur_line as usize);
            }
            _ => break,
        }
        // A separator with nothing after it (`:5,d`) closes the range on the cursor's line.
        if !matches!(
            s.get(i),
            Some(
                b'0'..=b'9'
                    | b'.'
                    | b'$'
                    | b'%'
                    | b'*'
                    | b'\''
                    | b'/'
                    | b'?'
                    | b'\\'
                    | b'+'
                    | b'-'
                    | b' '
            )
        ) {
            addrs.push(cur_line);
            break;
        }
    }
    let tail = rest[i..].trim_start();
    let Some(&last) = addrs.last() else {
        // No range, but a counted command still needs the cursor's line (`:d 3`).
        return Ok(counted(tail, cur_line, nlines)?.map_or_else(unchanged, ExRange::Line));
    };
    if tail.is_empty() {
        // Going to a line clamps past the end (`:99`), but not below the start (`:-5` on line 3 is E16).
        if last < 0 {
            return Err(ExRangeError::InvalidRange);
        }
        return Ok(ExRange::Goto(last.clamp(1, nlines) as usize));
    }
    let used = &addrs[addrs.len().saturating_sub(2)..];
    if used.iter().any(|&a| a < 0 || a > nlines) {
        return Err(ExRangeError::InvalidRange);
    }
    if let [a, b] = *used {
        if a > b {
            return Ok(ExRange::Backwards(format!("{b},{a}{tail}")));
        }
    }
    let range = match used {
        [1, b] if whole && *b == nlines => "%".to_string(),
        [a, b] => format!("{a},{b}"),
        _ => last.to_string(),
    };
    Ok(ExRange::Line(
        counted(tail, last, nlines)?.unwrap_or_else(|| format!("{range}{tail}")),
    ))
}

/// The 1-based line of the mark named by the byte `c` after a `'`: `Some(None)` for a mark that is not set
/// (or a file mark, which ruse does not keep), `None` when `c` names no mark.
fn mark_line(ws: &Workspace, c: u8, visual: Option<(i64, i64)>) -> Option<Option<i64>> {
    let view = &ws.focused().view;
    let pos = match c {
        b'a'..=b'z' => view.named_mark(c as char),
        b'<' => return Some(visual.map(|v| v.0)),
        b'>' => return Some(visual.map(|v| v.1)),
        b'[' => view.change_mark_start(),
        b']' => view.change_mark_end(),
        b'.' => view.last_change(),
        b'\'' | b'`' => view.context_mark(),
        b'A'..=b'Z' | b'0'..=b'9' => None,
        _ => return None,
    };
    let b = ws.focused().doc.bytes();
    Some(pos.map(|p| ruse_core::pos::line_of(b, p) as i64 + 1))
}

/// The pattern of a `/pat/` or `?pat?` address that starts at byte `from` of `s`: the text up to the next
/// unescaped `delim` (or the end of the line), and the offset just past that delimiter.
fn scan_pattern(s: &str, from: usize, delim: char) -> (&str, usize) {
    let mut chars = s[from..].char_indices();
    while let Some((j, c)) = chars.next() {
        if c == '\\' {
            chars.next();
        } else if c == delim {
            return (&s[from..from + j], from + j + c.len_utf8());
        }
    }
    (&s[from..], s.len())
}

/// The line a pattern address lands on: the first line after `from` that matches (`forward`) or the last
/// one before it, wrapping around the end of the buffer ('wrapscan'), so `from` itself matches last.
fn search_line(
    ws: &mut Workspace,
    pat: &str,
    from: i64,
    forward: bool,
) -> Result<i64, ExRangeError> {
    let hits = ws
        .global_marks(SubRange::WholeFile, pat, false)
        .map_err(|e| ExRangeError::BadPattern(crate::app::dispatch::regex_error_msg(&e)))?;
    let hits: Vec<i64> = hits.into_iter().map(|l| l as i64).collect();
    let hit = if forward {
        hits.iter().find(|&&l| l > from).or(hits.first())
    } else {
        hits.iter().rev().find(|&&l| l < from).or(hits.last())
    };
    let hit = hit.copied();
    hit.ok_or_else(|| ExRangeError::PatternNotFound(pat.to_string()))
}

/// For `:d`, `:y` (each `[x] [count]`), `:j[!] [count]` and `:>`/`:<` `[count]` (`:help :d`): the line in
/// canonical form, with a count turned into the range of `count` lines from `line2` (clamped to the
/// buffer, as Vim does) and the register after one space (`2,4d a`). `None` when `tail` is not one of
/// these commands or has no argument.
///
/// # Errors
/// [`ExRangeError::PositiveCount`] for a zero count, [`ExRangeError::Trailing`] for anything after the
/// register and count.
fn counted(tail: &str, line2: i64, nlines: i64) -> Result<Option<String>, ExRangeError> {
    let (verb, args) = match tail.as_bytes().first() {
        Some(&c @ (b'>' | b'<')) => {
            tail.split_at(tail.find(|x| x != c as char).unwrap_or(tail.len()))
        }
        _ => {
            let end = tail
                .find(|c: char| !c.is_ascii_alphabetic())
                .unwrap_or(tail.len());
            let bang = usize::from(tail[end..].starts_with('!'));
            tail.split_at(end + bang)
        }
    };
    let takes_register = match verb {
        "d" | "de" | "del" | "dele" | "delet" | "delete" | "y" | "ya" | "yan" | "yank" => true,
        "j" | "jo" | "joi" | "join" | "j!" | "jo!" | "joi!" | "join!" => false,
        _ if verb.starts_with(['>', '<']) => false,
        _ => return Ok(None),
    };
    let mut a = args.trim_start();
    if a.is_empty() {
        return Ok(None);
    }
    let mut reg = None;
    if takes_register {
        if let Some(c) = a
            .chars()
            .next()
            .filter(|c| c.is_ascii_alphabetic() || matches!(c, '"' | '-' | '_' | '*' | '+'))
        {
            reg = Some(c);
            a = a[1..].trim_start();
        }
    }
    let digits = a.find(|c: char| !c.is_ascii_digit()).unwrap_or(a.len());
    let count = if digits > 0 {
        let n: i64 = a[..digits].parse().unwrap_or(i64::MAX);
        if n == 0 {
            return Err(ExRangeError::PositiveCount);
        }
        a = a[digits..].trim_start();
        Some(n)
    } else {
        None
    };
    if !a.is_empty() {
        return Err(ExRangeError::Trailing(a.to_string()));
    }
    // Without a count the caller keeps its own range, and the parser reads the register.
    let Some(n) = count else {
        return Ok(None);
    };
    let last = line2.saturating_add(n - 1).min(nlines);
    let reg = reg.map(|c| format!(" {c}")).unwrap_or_default();
    Ok(Some(format!("{line2},{last}{verb}{reg}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolve_in(ws: &mut Workspace, line: &str) -> Result<ExRange, ExRangeError> {
        resolve_ex_range(line, ws, AddrPatterns::default()).range
    }

    /// #1079: the leading range resolves `.`, `$`, offsets and `;` to line numbers, and an address past the
    /// last line is E16 for a command but clamps for a bare `:N` (nvim v0.12.6).
    #[test]
    fn ex_ranges_resolve_and_refuse_past_the_end() {
        let resolve = |cursor_line: usize, line: &str| {
            let mut ws = Workspace::new(b"a\nb\nc\nd\n".to_vec());
            ws.place_focused_cursor(cursor_line * 2);
            resolve_in(&mut ws, line)
        };
        let line = |s: &str| Ok(ExRange::Line(s.to_string()));
        let e16 = Err(ExRangeError::InvalidRange);
        assert_eq!(resolve(0, "2,$d"), line("2,4d"));
        assert_eq!(resolve(1, ".,$d"), line("2,4d"));
        assert_eq!(resolve(1, ".,.+1d"), line("2,3d"));
        assert_eq!(resolve(0, ".+1,$-1d"), line("2,3d"));
        assert_eq!(resolve(0, "2;+1d"), line("2,3d"));
        assert_eq!(resolve(2, "-,+d"), line("2,4d"));
        assert_eq!(
            resolve(0, "1,2,3d"),
            line("2,3d"),
            "the last two addresses count"
        );
        assert_eq!(
            resolve(3, "2,d"),
            line("2,4d"),
            "a missing address is the cursor's line"
        );
        assert_eq!(resolve(0, "%d"), line("%d"));
        assert_eq!(resolve(0, "% d"), line("%d"));
        assert_eq!(resolve(0, "s/a/b/"), line("s/a/b/"));
        assert_eq!(resolve(0, "0d"), line("0d"));
        for past in [
            "2,6d",
            "5d",
            "2,6y",
            "2,6s/a/b/",
            "2,6>",
            "2,6m0",
            "2,6t0",
            "2,6normal x",
            "5put",
        ] {
            assert_eq!(resolve(0, past), e16, "{past}");
        }
        assert_eq!(resolve(0, "$-5,$d"), e16);
        assert_eq!(
            resolve(2, "-5"),
            e16,
            "a bare range above line 1 is still E16"
        );
        assert_eq!(resolve(0, "9"), Ok(ExRange::Goto(4)), "a bare range clamps");
        assert_eq!(resolve(0, "$"), Ok(ExRange::Goto(4)));
        assert_eq!(resolve(0, "%"), Ok(ExRange::Goto(4)));
        assert_eq!(resolve(2, "-"), Ok(ExRange::Goto(2)));
        assert_eq!(resolve(0, "0"), Ok(ExRange::Goto(1)));
        assert_eq!(resolve(0, "'<,'>d"), Err(ExRangeError::MarkNotSet));
        assert_eq!(resolve(0, "*d"), Err(ExRangeError::MarkNotSet));
        // A buffer without a final newline has the same lines; an empty buffer has one.
        let mut ws = Workspace::new(b"a\nb".to_vec());
        assert_eq!(resolve_in(&mut ws, "3d"), e16);
        assert_eq!(resolve_in(&mut ws, "2d"), line("2d"));
        let mut ws = Workspace::new(Vec::new());
        assert_eq!(resolve_in(&mut ws, "1d"), line("1d"));
        assert_eq!(resolve_in(&mut ws, "2d"), e16);
    }

    /// #608: marks, patterns and the last-pattern addresses, checked against nvim v0.12.6.
    #[test]
    fn mark_and_pattern_addresses_resolve() {
        let text = b"a1\nb2\na3\nb4\na5\n".to_vec();
        let line = |s: &str| Ok(ExRange::Line(s.to_string()));
        let mut ws = Workspace::new(text.clone());
        assert_eq!(resolve_in(&mut ws, "/b/d"), line("2d"));
        assert_eq!(resolve_in(&mut ws, "/b/;/b/d"), line("2,4d"));
        assert_eq!(resolve_in(&mut ws, "/b//a/d"), line("3d"));
        assert_eq!(resolve_in(&mut ws, "/b/+1d"), line("3d"));
        assert_eq!(resolve_in(&mut ws, "?b?d"), line("4d"), "backward wraps");
        assert_eq!(
            resolve_in(&mut ws, "/a1/d"),
            line("1d"),
            "the line itself last"
        );
        assert_eq!(resolve_in(&mut ws, "/b"), Ok(ExRange::Goto(2)));
        assert_eq!(
            resolve_in(&mut ws, "/zz/d"),
            Err(ExRangeError::PatternNotFound("zz".into()))
        );
        assert_eq!(
            resolve_in(&mut ws, "\\/d"),
            Err(ExRangeError::NoPreviousPattern)
        );
        assert_eq!(
            resolve_in(&mut ws, "\\&d"),
            Err(ExRangeError::NoPreviousSubstitute)
        );
        let pats = AddrPatterns {
            search: Some("a"),
            substitute: Some("b"),
        };
        let r = |ws: &mut Workspace, l: &str| resolve_ex_range(l, ws, pats).range;
        assert_eq!(r(&mut ws, "\\/d"), line("3d"));
        assert_eq!(r(&mut ws, "\\?d"), line("5d"));
        assert_eq!(r(&mut ws, "\\&d"), line("2d"));
        assert_eq!(
            r(&mut ws, "//d"),
            line("3d"),
            "an empty pattern is the last search"
        );
        let found = resolve_ex_range("?b?", &mut ws, pats);
        assert_eq!(found.search, Some(("b".to_string(), false)));
        assert_eq!(
            r(&mut ws, "/b/;//d"),
            line("2,4d"),
            "`//` is the pattern just searched"
        );
        let semi = resolve_ex_range("5;-2d", &mut ws, pats);
        assert_eq!(semi.range, Ok(ExRange::Backwards("3,5d".into())));
        assert_eq!(semi.cursor, Some(5), "`;` moves the cursor");
        // Marks.
        assert_eq!(resolve_in(&mut ws, "'a,'bd"), Err(ExRangeError::MarkNotSet));
        assert_eq!(resolve_in(&mut ws, "'Ad"), Err(ExRangeError::MarkNotSet));
        ws.place_focused_cursor(3);
        ws.apply(&ruse_core::Command::SetNamedMark('a'));
        ws.place_focused_cursor(9);
        ws.apply(&ruse_core::Command::SetNamedMark('b'));
        assert_eq!(resolve_in(&mut ws, "'a,'bd"), line("2,4d"));
        assert_eq!(
            resolve_in(&mut ws, "'b,'ad"),
            Ok(ExRange::Backwards("2,4d".into()))
        );
        assert_eq!(resolve_in(&mut ws, "'a"), Ok(ExRange::Goto(2)));
    }

    /// `:d`, `:y`, `:j`, `:>` take a register and a count after the command (nvim v0.12.6).
    #[test]
    fn a_count_after_the_command_is_a_range() {
        let resolve = |cursor_line: usize, line: &str| {
            let mut ws = Workspace::new(b"a1\nb2\na3\nb4\na5\n".to_vec());
            ws.place_focused_cursor(cursor_line * 3);
            resolve_in(&mut ws, line)
        };
        let line = |s: &str| Ok(ExRange::Line(s.to_string()));
        assert_eq!(resolve(0, "d 3"), line("1,3d"));
        assert_eq!(resolve(0, "2d 2"), line("2,3d"));
        assert_eq!(resolve(0, "2,3d 2"), line("3,4d"));
        assert_eq!(resolve(0, "d a 2"), line("1,2d a"));
        assert_eq!(resolve(0, "4d 9"), line("4,5d"), "clamped to the buffer");
        assert_eq!(resolve(0, "%d 2"), line("5,5d"));
        assert_eq!(resolve(1, "y 2"), line("2,3y"));
        assert_eq!(resolve(0, "j 3"), line("1,3j"));
        assert_eq!(resolve(0, "j! 3"), line("1,3j!"));
        assert_eq!(resolve(0, "> 2"), line("1,2>"));
        assert_eq!(resolve(0, ">>2"), line("1,2>>"));
        assert_eq!(
            resolve(0, "d a"),
            line("d a"),
            "a register alone is the parser's"
        );
        assert_eq!(resolve(0, "d 0"), Err(ExRangeError::PositiveCount));
        assert_eq!(resolve(0, "d x y"), Err(ExRangeError::Trailing("y".into())));
        assert_eq!(resolve(0, "j x"), Err(ExRangeError::Trailing("x".into())));
        assert_eq!(resolve(0, "> x"), Err(ExRangeError::Trailing("x".into())));
        assert_eq!(resolve(0, "dig"), line("dig"), "another verb is left alone");
    }
}
