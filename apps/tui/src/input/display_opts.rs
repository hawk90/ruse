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
