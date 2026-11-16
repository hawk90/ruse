//! Applying a `:set` value: type checks, `+=`/`-=`/`^=`, and validation. Split from `set_cmd.rs` (#1388).

use super::*;

/// Why a value cannot be assigned to an option (the message prefix; the argument text follows it).
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ValueError {
    /// `E487: Argument must be positive`.
    Positive,
    /// `E474: Invalid argument`.
    Invalid,
    /// A string option's own check failed (`E539: Illegal character <x>`, `E537`, …).
    Message(String),
}

impl ValueError {
    #[must_use]
    pub fn message(&self) -> &str {
        match self {
            ValueError::Positive => "E487: Argument must be positive",
            ValueError::Invalid => E474,
            ValueError::Message(m) => m,
        }
    }
}

/// The value `op` makes of `current` with `val`, checked against the option's rules.
///
/// # Errors
/// The [`ValueError`] nvim raises for it.
pub fn assign_value(
    def: &OptDef,
    current: OptVal,
    op: AssignOp,
    val: &RawVal,
) -> Result<OptVal, ValueError> {
    match (def.kind, current, val) {
        (OptKind::Num { min, max }, OptVal::Num(cur), RawVal::Num(n)) => {
            let v = match op {
                AssignOp::Set => Some(*n),
                AssignOp::Add => cur.checked_add(*n),
                AssignOp::Sub => cur.checked_sub(*n),
                AssignOp::Prepend => cur.checked_mul(*n),
            }
            .ok_or(ValueError::Invalid)?;
            if !(INT_MIN..=INT_MAX).contains(&v) {
                return Err(ValueError::Invalid);
            }
            // 'scrolljump' below -100, and a negative 'scroll', are nvim's E49, not E487.
            if matches!(def.opt, Opt::ScrollJump | Opt::Scroll) && v < min {
                return Err(ValueError::Message("E49: Invalid scroll size".into()));
            }
            // Below the minimum is "not positive" (`'tabstop'` 0 too, as nvim says).
            if v < min {
                return Err(ValueError::Positive);
            }
            if v > max {
                return Err(ValueError::Invalid);
            }
            Ok(OptVal::Num(v))
        }
        (OptKind::Str { values, .. }, OptVal::Str(cur), RawVal::Str(v)) => {
            let new = match op {
                AssignOp::Set => v.clone(),
                AssignOp::Add => format!("{cur}{v}"),
                AssignOp::Prepend => format!("{v}{cur}"),
                AssignOp::Sub => cur.replacen(v.as_str(), "", 1),
            };
            values
                .iter()
                .find(|&&x| x == new)
                .map(|&x| OptVal::Str(x))
                .ok_or(ValueError::Invalid)
        }
        (OptKind::Text { list, check }, OptVal::Text(cur), RawVal::Str(v)) => {
            let new = combine(list, &cur, op, v);
            let checked = match check {
                TextCheck::None => Ok(()),
                TextCheck::FormatOptions => ruse_core::editor::check_formatoptions(&new),
                TextCheck::Comments => ruse_core::editor::check_comments(&new),
                TextCheck::CommentString => {
                    if new.is_empty() || new.contains("%s") {
                        Ok(())
                    } else {
                        Err(ruse_core::editor::OptionValueError::NoPercentS)
                    }
                }
                TextCheck::TabStops => {
                    return ruse_core::VarStops::parse(&new)
                        .map(|_| OptVal::Text(Cow::Owned(new)))
                        .ok_or(ValueError::Invalid);
                }
                TextCheck::BackupCopy | TextCheck::BackupExt | TextCheck::FileFormats => {
                    let ok = match check {
                        TextCheck::BackupCopy => backupcopy_valid(&new),
                        TextCheck::FileFormats => new
                            .split(',')
                            .all(|f| f.is_empty() || FILE_FORMATS.contains(&f)),
                        _ => !new.contains('/'),
                    };
                    return if ok {
                        Ok(OptVal::Text(Cow::Owned(new)))
                    } else {
                        Err(ValueError::Invalid)
                    };
                }
                TextCheck::FoldOpen => {
                    return if foldopen_valid(&new) {
                        Ok(OptVal::Text(Cow::Owned(new)))
                    } else {
                        Err(ValueError::Invalid)
                    };
                }
                TextCheck::MessagesOpt => {
                    return messagesopt_history(&new)
                        .map(|_| OptVal::Text(Cow::Owned(new)))
                        .ok_or(ValueError::Invalid);
                }
                TextCheck::ListChars
                | TextCheck::BreakIndentOpt
                | TextCheck::ShowBreak
                | TextCheck::Display
                | TextCheck::SignColumn
                | TextCheck::FillChars
                | TextCheck::CursorLineOpt
                | TextCheck::ColorColumn
                | TextCheck::FoldColumn => {
                    display_check(check, &new).map_err(|e| ValueError::Message(e.to_string()))?;
                    Ok(())
                }
            };
            checked.map_err(|e| ValueError::Message(e.to_string()))?;
            Ok(OptVal::Text(Cow::Owned(new)))
        }
        _ => Err(ValueError::Invalid),
    }
}

/// The display options' value checks (#615, #616), as nvim's `did_set_*` callbacks make them.
fn display_check(
    check: TextCheck,
    v: &str,
) -> Result<(), crate::input::display_opts::DisplayOptError> {
    use crate::input::display_opts as d;
    match check {
        TextCheck::ListChars => d::parse_listchars(v).map(|_| ()),
        TextCheck::BreakIndentOpt => d::parse_briopt(v).map(|_| ()),
        TextCheck::ShowBreak => d::check_showbreak(v),
        TextCheck::Display => d::check_display(v),
        TextCheck::SignColumn => d::parse_signcolumn(v).map(|_| ()),
        TextCheck::FillChars => d::parse_fillchars(v).map(|_| ()),
        TextCheck::CursorLineOpt => d::parse_culopt(v).map(|_| ()),
        TextCheck::ColorColumn => d::parse_colorcolumn(v).map(|_| ()),
        TextCheck::FoldColumn => d::parse_foldcolumn(v).map(|_| ()),
        _ => Ok(()),
    }
}

/// `s` with each flag that appears again later dropped.
fn dedup_flags(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    chars
        .iter()
        .enumerate()
        .filter(|&(i, c)| !chars[i + 1..].contains(c))
        .map(|(_, c)| c)
        .collect()
}

/// `cur` combined with `v` by `op` the way nvim's `do_set_option_string` combines a string option.
fn combine(list: ListKind, cur: &str, op: AssignOp, v: &str) -> String {
    match (op, list) {
        // A flag list keeps the LAST of a flag that appears twice (nvim's `stropt_remove_dupflags`).
        (AssignOp::Set, ListKind::Flags) => dedup_flags(v),
        (AssignOp::Set, _) => v.to_string(),
        (AssignOp::Add, ListKind::Flags) => dedup_flags(&format!("{cur}{v}")),
        (AssignOp::Prepend, ListKind::Flags) => dedup_flags(&format!("{v}{cur}")),
        (AssignOp::Add | AssignOp::Prepend, ListKind::Comma) => {
            if v.is_empty() || cur.split(',').any(|item| item == v) {
                return cur.to_string();
            }
            match (cur.is_empty(), op) {
                (true, _) => v.to_string(),
                (false, AssignOp::Add) => format!("{cur},{v}"),
                (false, _) => format!("{v},{cur}"),
            }
        }
        (AssignOp::Add, ListKind::Plain) => format!("{cur}{v}"),
        (AssignOp::Prepend, ListKind::Plain) => format!("{v}{cur}"),
        (AssignOp::Sub, ListKind::Comma) => {
            let items: Vec<&str> = cur.split(',').collect();
            match items.iter().position(|item| *item == v) {
                Some(i) => {
                    let mut items = items;
                    items.remove(i);
                    items.join(",")
                }
                None => cur.to_string(),
            }
        }
        (AssignOp::Sub, ListKind::Flags) => dedup_flags(&cur.replacen(v, "", 1)),
        (AssignOp::Sub, _) => cur.replacen(v, "", 1),
    }
}
