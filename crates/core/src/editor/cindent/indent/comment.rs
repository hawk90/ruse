//! The comment branch of nvim's `get_c_indent`: a line inside a `/* */` comment, lined up by the
//! three-piece `'comments'` entries.

use super::super::lex::{at, ncmp, skipwhite};
use super::super::options::getdigits;
use super::super::{Ci, Pos};

impl<L: AsRef<[u8]>> Ci<'_, L> {
    /// The comment branch of `get_c_indent`: a line inside a `/* */` comment that started at `cpos`.
    pub(super) fn comment_indent(&self, theline: &[u8], cpos: Pos, cur_curpos: Pos) -> i32 {
        let mut amount = i32::try_from(self.vcol(cpos.lnum, cpos.col)).unwrap_or(i32::MAX);
        let mut lead_start: Vec<u8> = Vec::new();
        let mut lead_middle: Vec<u8> = Vec::new();
        let mut lead_start_len = 2usize;
        let mut lead_middle_len = 1usize;
        let mut start_align = 0u8;
        let mut start_off = 0i32;
        let mut done = false;
        let com = self.e.comments.as_bytes();
        let mut p = 0usize;
        let lnum = self.cur.lnum;
        'entries: while p < com.len() {
            let CommentPart {
                what,
                align,
                off,
                lead: lead_end,
            } = next_comment_part(com, &mut p);
            let lead_end_len = lead_end.len();
            match what {
                b's' => {
                    lead_start.clone_from(&lead_end);
                    lead_start_len = lead_end_len;
                    start_off = off;
                    start_align = align;
                }
                b'm' => {
                    lead_middle.clone_from(&lead_end);
                    lead_middle_len = lead_end_len;
                }
                b'e' => {
                    if ncmp(theline, &lead_middle, lead_middle_len)
                        && !ncmp(theline, &lead_end, lead_end_len)
                    {
                        done = true;
                        if lnum > 1 {
                            let pl = self.line(lnum - 1);
                            let look = &pl[skipwhite(pl, 0)..];
                            if ncmp(look, &lead_start, lead_start_len) {
                                amount = self.indent_of(lnum - 1);
                            } else if ncmp(look, &lead_middle, lead_middle_len) {
                                amount = self.indent_of(lnum - 1);
                                break 'entries;
                            } else if !ncmp(
                                &self.line(cpos.lnum)[cpos.col.min(self.line(cpos.lnum).len())..],
                                &lead_start,
                                lead_start_len,
                            ) {
                                continue 'entries;
                            }
                        }
                        if start_off != 0 {
                            amount += start_off;
                        } else if start_align == b'r' {
                            amount += lead_start.len() as i32 - lead_middle.len() as i32;
                        }
                        break 'entries;
                    }
                    if !ncmp(theline, &lead_middle, lead_middle_len)
                        && ncmp(theline, &lead_end, lead_end_len)
                    {
                        amount = self.indent_of(lnum - 1);
                        if off != 0 {
                            amount += off;
                        } else if align == b'r' {
                            amount += lead_start.len() as i32 - lead_middle.len() as i32;
                        }
                        done = true;
                        break 'entries;
                    }
                }
                _ => {}
            }
        }
        if done {
        } else if at(theline, 0) == b'*' {
            amount += 1;
        } else {
            amount = self.comment_text_amount(cpos, cur_curpos);
        }
        amount
    }

    /// `get_c_indent`, the comment branch when no `'comments'` entry decided it and the line does not
    /// start with `*`: "If we are more than one line away from the comment opener, take the indent of
    /// the previous non-empty line. If 'cino' has "CO" and we are just below the comment opener and
    /// there are any white characters after it line up with the text after it; otherwise, add the
    /// amount specified by "c" in 'cino'".
    fn comment_text_amount(&self, mut cpos: Pos, cur_curpos: Pos) -> i32 {
        let o = self.o();
        let mut amount = -1;
        let mut l = cur_curpos.lnum - 1;
        while l > cpos.lnum {
            if !self.linewhite(l) {
                amount = self.indent_of(l);
                break;
            }
            l -= 1;
        }
        if amount == -1 {
            let start = self.line(cpos.lnum);
            let look = cpos.col + 2;
            if o.in_comment2 == 0 && at(start, look) != 0 {
                cpos.col = skipwhite(start, look);
            }
            amount = i32::try_from(self.vcol(cpos.lnum, cpos.col)).unwrap_or(i32::MAX);
            if o.in_comment2 != 0 || at(start, look) == 0 {
                amount += o.in_comment;
            }
        }
        amount
    }
}

/// One `'comments'` entry as the comment branch of `get_c_indent` reads it: the flags before the
/// `:` (`what` is `s`, `m` or `e`, `align` is `l` or `r`, `off` the offset) and the leader after it.
struct CommentPart {
    what: u8,
    align: u8,
    off: i32,
    lead: Vec<u8>,
}

/// The flag loop of `get_c_indent`'s comment branch, then `copy_option_part` up to the next `,` (a
/// `\,` is a literal comma): the entry of `com` at `*p`, with `*p` moved past it.
fn next_comment_part(com: &[u8], p: &mut usize) -> CommentPart {
    let mut align = 0u8;
    let mut off = 0i32;
    let mut what = 0u8;
    while *p < com.len() && com[*p] != b':' {
        let c = com[*p];
        if c == b's' || c == b'e' || c == b'm' {
            what = c;
            *p += 1;
        } else if c == b'l' || c == b'r' {
            align = c;
            *p += 1;
        } else if c.is_ascii_digit() || c == b'-' {
            let before = *p;
            off = i32::try_from(getdigits(com, p)).unwrap_or(0);
            if *p == before {
                *p += 1; // a lone `-`: nvim would spin here
            }
        } else {
            *p += 1;
        }
    }
    if *p < com.len() && com[*p] == b':' {
        *p += 1;
    }
    let mut lead: Vec<u8> = Vec::new();
    while *p < com.len() && com[*p] != b',' {
        if com[*p] == b'\\' && *p + 1 < com.len() && com[*p + 1] == b',' {
            *p += 1;
        }
        lead.push(com[*p]);
        *p += 1;
    }
    if *p < com.len() {
        *p += 1;
    }
    CommentPart {
        what,
        align,
        off,
        lead,
    }
}
