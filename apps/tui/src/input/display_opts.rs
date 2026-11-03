//! The values of the display options `:set` checks (#615, #616): `'listchars'`, `'breakindentopt'`,
//! `'showbreak'`, `'signcolumn'` and `'display'`, parsed as nvim v0.12.6 checks them. Pure option
//! grammar, so the `:set` table can check a value without reaching into the renderer, which reads the
//! parsed forms.

/// nvim's default `'breakat'`.
pub(crate) const BREAKAT_DEFAULT: &str = " \t!@*-+;:,./?";

/// nvim's default `'listchars'`.
pub(crate) const LISTCHARS_DEFAULT: &str = "tab:> ,trail:-,nbsp:+";

/// Why a display option's value is refused (nvim's message, without the `: {arg}` tail `:set` adds).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum DisplayOptError {
    /// `E474: Invalid argument`.
    Invalid,
    /// `E1511: Wrong number of characters for field "{field}"`.
    WrongCount(String),
    /// `E1512: Wrong character width for field "{field}"`.
    WrongWidth(String),
    /// `E595: 'showbreak' contains unprintable or wide character`.
    ShowBreakWide,
}

impl std::fmt::Display for DisplayOptError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DisplayOptError::Invalid => f.write_str("E474: Invalid argument"),
            DisplayOptError::WrongCount(n) => {
                write!(f, "E1511: Wrong number of characters for field \"{n}\"")
            }
            DisplayOptError::WrongWidth(n) => {
                write!(f, "E1512: Wrong character width for field \"{n}\"")
            }
            DisplayOptError::ShowBreakWide => {
                f.write_str("E595: 'showbreak' contains unprintable or wide character")
            }
        }
    }
}

/// `'listchars'`, parsed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ListChars {
    pub eol: Option<char>,
    /// `tab:xy[z]`.
    pub tab: Option<(char, char, Option<char>)>,
    pub space: Option<char>,
    pub multispace: Vec<char>,
    pub lead: Option<char>,
    pub leadmultispace: Vec<char>,
    pub trail: Option<char>,
    pub extends: Option<char>,
    pub precedes: Option<char>,
    pub nbsp: Option<char>,
}

/// Parse `'listchars'` the way nvim's `set_chars_option` checks it. The error is the message without the
/// `: {arg}` tail `:set` adds.
pub(crate) fn parse_listchars(s: &str) -> Result<ListChars, DisplayOptError> {
    let mut lc = ListChars::default();
    if s.is_empty() {
        return Ok(lc);
    }
    for item in s.split(',') {
        let Some((name, value)) = item.split_once(':') else {
            return Err(DisplayOptError::Invalid);
        };
        let chars: Vec<char> = value.chars().collect();
        let wrong_count = || DisplayOptError::WrongCount(name.to_string());
        let wrong_width = || DisplayOptError::WrongWidth(name.to_string());
        let narrow = |c: &char| unicode_width::UnicodeWidthChar::width(*c) == Some(1);
        let one = || match chars.as_slice() {
            [c] if narrow(c) => Ok(*c),
            [_] => Err(wrong_width()),
            _ => Err(wrong_count()),
        };
        match name {
            "eol" => lc.eol = Some(one()?),
            "space" => lc.space = Some(one()?),
            "lead" => lc.lead = Some(one()?),
            "trail" => lc.trail = Some(one()?),
            "extends" => lc.extends = Some(one()?),
            "precedes" => lc.precedes = Some(one()?),
            "nbsp" => lc.nbsp = Some(one()?),
            // `conceal:c` is read by 'conceallevel', which ruse does not have: accepted, unused.
            "conceal" => {
                one()?;
            }
            "tab" => {
                if !(2..=3).contains(&chars.len()) {
                    return Err(wrong_count());
                }
                if !chars.iter().all(narrow) {
                    return Err(wrong_width());
                }
                lc.tab = Some((chars[0], chars[1], chars.get(2).copied()));
            }
            "multispace" | "leadmultispace" => {
                if chars.is_empty() {
                    return Err(wrong_count());
                }
                if !chars.iter().all(narrow) {
                    return Err(wrong_width());
                }
                if name == "multispace" {
                    lc.multispace = chars;
                } else {
                    lc.leadmultispace = chars;
                }
            }
            _ => return Err(DisplayOptError::Invalid),
        }
    }
    Ok(lc)
}

/// `'breakindentopt'`, parsed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct BriOpt {
    /// `min:{n}`: the text width a wrapped row keeps at least (default 20).
    pub min: i64,
    /// `shift:{n}`: added to the indent (may be negative).
    pub shift: i64,
    /// `sbr`: the `'showbreak'` goes before the indent.
    pub sbr: bool,
    /// `list:{n}`: extra indent for a numbered list (read but not honoured; see [`check_briopt`]).
    pub list: i64,
    /// `column:{n}`: indent continuation rows to this column instead.
    pub column: i64,
}

impl Default for BriOpt {
    fn default() -> BriOpt {
        BriOpt {
            min: 20,
            shift: 0,
            sbr: false,
            list: 0,
            column: 0,
        }
    }
}

/// Parse `'breakindentopt'` (nvim's `briopt_check`): `E474` on anything else.
pub(crate) fn parse_briopt(s: &str) -> Result<BriOpt, DisplayOptError> {
    let mut o = BriOpt::default();
    let bad = || DisplayOptError::Invalid;
    for item in s.split(',').filter(|i| !i.is_empty()) {
        if item == "sbr" {
            o.sbr = true;
            continue;
        }
        let (name, n) = item.split_once(':').ok_or_else(bad)?;
        let digits = n.strip_prefix('-').unwrap_or(n);
        if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
            return Err(bad());
        }
        let v: i64 = n.parse().map_err(|_| bad())?;
        match name {
            "min" => o.min = v,
            "shift" => o.shift = v,
            "list" => o.list = v,
            "column" => o.column = v,
            _ => return Err(bad()),
        }
    }
    Ok(o)
}

/// nvim's `'showbreak'` check: every character one cell wide (`E595`).
pub(crate) fn check_showbreak(s: &str) -> Result<(), DisplayOptError> {
    if s.chars()
        .all(|c| c == '\t' || unicode_width::UnicodeWidthChar::width(c) == Some(1))
    {
        Ok(())
    } else {
        Err(DisplayOptError::ShowBreakWide)
    }
}

/// `'signcolumn'`, parsed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SignColumn {
    No,
    /// Always, `n` signs wide.
    Yes(usize),
    /// When the buffer has signs: at least `min`, at most `max` signs wide.
    Auto {
        min: usize,
        max: usize,
    },
    /// In the number column (when it shows; else as `auto`).
    Number,
}

/// Parse `'signcolumn'` (nvim's `check_signcolumn`).
pub(crate) fn parse_signcolumn(s: &str) -> Result<SignColumn, DisplayOptError> {
    let bad = || DisplayOptError::Invalid;
    let digit = |t: &str| match t.as_bytes() {
        [d @ b'1'..=b'9'] => Ok(usize::from(d - b'0')),
        _ => Err(bad()),
    };
    Ok(match s {
        "no" => SignColumn::No,
        "yes" => SignColumn::Yes(1),
        "auto" => SignColumn::Auto { min: 0, max: 1 },
        "number" => SignColumn::Number,
        _ => {
            if let Some(n) = s.strip_prefix("yes:") {
                SignColumn::Yes(digit(n)?)
            } else if let Some(r) = s.strip_prefix("auto:") {
                match r.split_once('-') {
                    Some((a, b)) => {
                        let (min, max) = (digit(a)?, digit(b)?);
                        if min >= max {
                            return Err(bad());
                        }
                        SignColumn::Auto { min, max }
                    }
                    None => SignColumn::Auto {
                        min: 0,
                        max: digit(r)?,
                    },
                }
            } else {
                return Err(bad());
            }
        }
    })
}

/// Parse `'display'`: a comma list of `lastline`, `truncate`, `uhex` and `msgsep` (`E474` otherwise).
pub(crate) fn check_display(s: &str) -> Result<(), DisplayOptError> {
    if s.split(',')
        .filter(|f| !f.is_empty())
        .all(|f| matches!(f, "lastline" | "truncate" | "uhex" | "msgsep"))
    {
        Ok(())
    } else {
        Err(DisplayOptError::Invalid)
    }
}

/// `'fillchars'`, parsed (#616): the characters ruse draws, with nvim v0.12.6's defaults (UTF-8, single
/// `'ambiwidth'`). The status-line, window-bar, diff and message fields are accepted and kept but unused:
/// ruse has no per-window status line, window bar or diff mode.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FillChars {
    /// The vertical split separator.
    pub vert: char,
    /// The horizontal split divider (nvim's `horiz`, drawn under 'laststatus' 3).
    pub horiz: char,
    /// The fill after a closed fold's text.
    pub fold: char,
    /// The fold column's open-fold, closed-fold and inside-fold marks.
    pub foldopen: char,
    pub foldclose: char,
    pub foldsep: char,
    /// The fold column's mark inside a nested fold too deep to show (nvim shows the level digit).
    pub foldinner: Option<char>,
    /// The rows after the last line.
    pub eob: char,
    /// The `'display'` `lastline` / `truncate` mark.
    pub lastline: char,
}

impl Default for FillChars {
    fn default() -> FillChars {
        FillChars {
            vert: '\u{2502}',
            horiz: '\u{2500}',
            fold: '\u{b7}',
            foldopen: '-',
            foldclose: '+',
            foldsep: '\u{2502}',
            foldinner: None,
            eob: '~',
            lastline: '@',
        }
    }
}

/// Parse `'fillchars'` the way nvim's `set_chars_option` checks it: `name:c` items, one one-cell
/// character each (`E1511` / `E1512`), an unknown name `E474`. Fields not given keep their default.
pub(crate) fn parse_fillchars(s: &str) -> Result<FillChars, DisplayOptError> {
    let mut fc = FillChars::default();
    if s.is_empty() {
        return Ok(fc);
    }
    for item in s.split(',') {
        let Some((name, value)) = item.split_once(':') else {
            return Err(DisplayOptError::Invalid);
        };
        let chars: Vec<char> = value.chars().collect();
        let c = match chars.as_slice() {
            [c] if unicode_width::UnicodeWidthChar::width(*c) == Some(1) => *c,
            [_] => return Err(DisplayOptError::WrongWidth(name.to_string())),
            _ => return Err(DisplayOptError::WrongCount(name.to_string())),
        };
        match name {
            "vert" => fc.vert = c,
            "horiz" => fc.horiz = c,
            "fold" => fc.fold = c,
            "foldopen" => fc.foldopen = c,
            "foldclose" => fc.foldclose = c,
            "foldsep" => fc.foldsep = c,
            "foldinner" => fc.foldinner = Some(c),
            "eob" => fc.eob = c,
            "lastline" => fc.lastline = c,
            "stl" | "stlnc" | "wbr" | "horizup" | "horizdown" | "vertleft" | "vertright"
            | "verthoriz" | "diff" | "msgsep" | "trunc" | "truncrl" => {}
            _ => return Err(DisplayOptError::Invalid),
        }
    }
    Ok(fc)
}

/// `'cursorlineopt'`, parsed (#616).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CursorLineOpt {
    /// `line`: the whole text line (every screen row of it).
    pub line: bool,
    /// `screenline`: only the screen row the cursor is on.
    pub screenline: bool,
    /// `number`: the line number (`CursorLineNr`).
    pub number: bool,
}

/// Parse `'cursorlineopt'` (nvim's `did_set_cursorlineopt`): a comma list of `line`, `screenline`,
/// `number` and `both` (`line,number`); `line` with `screenline`, or nothing, is `E474`.
pub(crate) fn parse_culopt(s: &str) -> Result<CursorLineOpt, DisplayOptError> {
    let mut o = CursorLineOpt {
        line: false,
        screenline: false,
        number: false,
    };
    if s.is_empty() {
        return Err(DisplayOptError::Invalid);
    }
    for item in s.split(',') {
        match item {
            "line" => o.line = true,
            "screenline" => o.screenline = true,
            "number" => o.number = true,
            "both" => {
                o.line = true;
                o.number = true;
            }
            _ => return Err(DisplayOptError::Invalid),
        }
    }
    if o.line && o.screenline {
        return Err(DisplayOptError::Invalid);
    }
    Ok(o)
}

/// One `'colorcolumn'` entry: a screen column (1-based), or an offset from `'textwidth'`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ColorColumn {
    At(i64),
    FromTextWidth(i64),
}

/// Parse `'colorcolumn'` (nvim's `check_colorcolumn`): a comma list of numbers, each optionally `+` / `-`
/// prefixed (relative to `'textwidth'`); anything else `E474`.
pub(crate) fn parse_colorcolumn(s: &str) -> Result<Vec<ColorColumn>, DisplayOptError> {
    let mut out = Vec::new();
    if s.is_empty() {
        return Ok(out);
    }
    for item in s.split(',') {
        let (rel, digits) = match item.as_bytes().first() {
            Some(b'+' | b'-') => (true, &item[1..]),
            _ => (false, item),
        };
        if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
            return Err(DisplayOptError::Invalid);
        }
        let n: i64 = digits.parse().map_err(|_| DisplayOptError::Invalid)?;
        out.push(if rel {
            ColorColumn::FromTextWidth(if item.starts_with('-') { -n } else { n })
        } else {
            ColorColumn::At(n)
        });
    }
    Ok(out)
}

/// `'foldcolumn'`, parsed (#616).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FoldColumn {
    /// Always this many cells (0-9).
    Fixed(usize),
    /// As many cells as the deepest fold needs, at most `max` (1-9).
    Auto(usize),
}

/// Parse `'foldcolumn'` (nvim's `did_set_foldcolumn`): `0`-`9`, `auto` (`auto:1`) or `auto:1`-`auto:9`.
pub(crate) fn parse_foldcolumn(s: &str) -> Result<FoldColumn, DisplayOptError> {
    let digit = |t: &str, min: u8| match t.as_bytes() {
        [d @ b'0'..=b'9'] if *d >= min => Ok(usize::from(d - b'0')),
        _ => Err(DisplayOptError::Invalid),
    };
    match s {
        "auto" => Ok(FoldColumn::Auto(1)),
        _ => match s.strip_prefix("auto:") {
            Some(n) => digit(n, b'1').map(FoldColumn::Auto),
            None => digit(s, b'0').map(FoldColumn::Fixed),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// nvim v0.12.6's `:set` checks (`nvim_exec2`, `-u NONE`).
    #[test]
    fn the_616_options_check_their_values_like_nvim() {
        let fc = parse_fillchars("eob:x").unwrap();
        assert_eq!((fc.eob, fc.vert, fc.fold), ('x', '\u{2502}', '\u{b7}'));
        assert_eq!(
            parse_fillchars("eob:xy"),
            Err(DisplayOptError::WrongCount("eob".into()))
        );
        assert_eq!(
            parse_fillchars("eob:"),
            Err(DisplayOptError::WrongCount("eob".into()))
        );
        assert_eq!(
            parse_fillchars("eob:\u{4e2d}"),
            Err(DisplayOptError::WrongWidth("eob".into()))
        );
        assert_eq!(parse_fillchars("bad:x"), Err(DisplayOptError::Invalid));
        assert!(parse_fillchars("foldinner:x,trunc:x,truncrl:y,lastline:z").is_ok());
        assert!(parse_fillchars(
            "stl:x,stlnc:y,wbr:z,horiz:-,horizup:a,horizdown:b,vertleft:c,vertright:d,verthoriz:e,diff:f,msgsep:g"
        )
        .is_ok());
        assert_eq!(
            parse_culopt("both").unwrap(),
            CursorLineOpt {
                line: true,
                screenline: false,
                number: true
            }
        );
        for bad in ["line,screenline", "x", "", "both,screenline"] {
            assert_eq!(parse_culopt(bad), Err(DisplayOptError::Invalid), "{bad}");
        }
        assert!(parse_culopt("number,line").is_ok() && parse_culopt("both,number").is_ok());
        assert_eq!(
            parse_colorcolumn("1,+2,-3").unwrap(),
            [
                ColorColumn::At(1),
                ColorColumn::FromTextWidth(2),
                ColorColumn::FromTextWidth(-3)
            ]
        );
        assert!(parse_colorcolumn("0").is_ok() && parse_colorcolumn("-0").is_ok());
        for bad in ["x", "+", "1,,2"] {
            assert_eq!(
                parse_colorcolumn(bad),
                Err(DisplayOptError::Invalid),
                "{bad}"
            );
        }
        assert_eq!(parse_foldcolumn("auto"), Ok(FoldColumn::Auto(1)));
        assert_eq!(parse_foldcolumn("auto:9"), Ok(FoldColumn::Auto(9)));
        assert_eq!(parse_foldcolumn("9"), Ok(FoldColumn::Fixed(9)));
        for bad in ["auto:10", "10", "auto:0", "x", "-1"] {
            assert_eq!(
                parse_foldcolumn(bad),
                Err(DisplayOptError::Invalid),
                "{bad}"
            );
        }
    }
}
