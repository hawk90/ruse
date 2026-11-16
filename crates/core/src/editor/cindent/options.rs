//! `'cinoptions'` parsed into [`CinOptions`], as nvim's `parse_cino` does.

use super::lex::at;

/// `'cinoptions'` parsed (nvim's `b_ind_*` values, in columns), for a given `'shiftwidth'`.
#[allow(missing_docs)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CinOptions {
    pub level: i32,
    pub open_imag: i32,
    pub no_brace: i32,
    pub first_open: i32,
    pub open_extra: i32,
    pub close_extra: i32,
    pub open_left_imag: i32,
    pub jump_label: i32,
    pub case: i32,
    pub case_code: i32,
    pub case_break: i32,
    pub scopedecl: i32,
    pub scopedecl_code: i32,
    pub param: i32,
    pub func_type: i32,
    pub cpp_baseclass: i32,
    pub continuation: i32,
    pub unclosed: i32,
    pub unclosed2: i32,
    pub unclosed_noignore: i32,
    pub unclosed_wrapped: i32,
    pub unclosed_whiteok: i32,
    pub matching_paren: i32,
    pub paren_prev: i32,
    pub comment: i32,
    pub in_comment: i32,
    pub in_comment2: i32,
    pub maxparen: i32,
    pub maxcomment: i32,
    pub java: i32,
    pub js: i32,
    pub keep_case_label: i32,
    pub cpp_namespace: i32,
    pub if_for_while: i32,
    pub hash_comment: i32,
    pub cpp_extern_c: i32,
    pub pragma: i32,
}

impl CinOptions {
    /// `parse_cino`: the defaults for shiftwidth `sw`, then each `{flag}{N}` / `{flag}{N}s` /
    /// `{flag}-{N}` / `{flag}{N}.{M}s` item of `cino` (comma-separated, unknown flags ignored).
    #[must_use]
    pub fn parse(cino: &str, sw: usize) -> CinOptions {
        let sw = i32::try_from(sw).unwrap_or(i32::MAX);
        let mut o = CinOptions {
            level: sw,
            open_imag: 0,
            no_brace: 0,
            first_open: 0,
            open_extra: 0,
            close_extra: 0,
            open_left_imag: 0,
            jump_label: -1,
            case: sw,
            case_code: sw,
            case_break: 0,
            scopedecl: sw,
            scopedecl_code: sw,
            param: sw,
            func_type: sw,
            cpp_baseclass: sw,
            continuation: sw,
            unclosed: sw.saturating_mul(2),
            unclosed2: sw,
            unclosed_noignore: 0,
            unclosed_wrapped: 0,
            unclosed_whiteok: 0,
            matching_paren: 0,
            paren_prev: 0,
            comment: 0,
            in_comment: 3,
            in_comment2: 0,
            maxparen: 20,
            maxcomment: 70,
            java: 0,
            js: 0,
            keep_case_label: 0,
            cpp_namespace: 0,
            if_for_while: 0,
            hash_comment: 0,
            cpp_extern_c: 0,
            pragma: 0,
        };
        let s = cino.as_bytes();
        let sw = i64::from(sw);
        let mut p = 0usize;
        let mut fraction: i64 = 0;
        while p < s.len() {
            let l = p;
            p += 1;
            if at(s, p) == b'-' {
                p += 1;
            }
            let digits_start = p;
            let mut n = getdigits(s, &mut p);
            let mut divider: i64 = 0;
            if at(s, p) == b'.' {
                p += 1;
                fraction = atoi(s, p);
                while at(s, p).is_ascii_digit() {
                    p += 1;
                    divider = if divider != 0 { divider * 10 } else { 10 };
                }
            }
            if at(s, p) == b's' {
                if p == digits_start {
                    n = sw;
                } else {
                    n = n.saturating_mul(sw);
                    if divider != 0 {
                        n = n.saturating_add((sw * fraction + divider / 2) / divider);
                    }
                }
                p += 1;
            }
            if at(s, l + 1) == b'-' {
                n = -n;
            }
            let n = i32::try_from(n.clamp(i64::from(i32::MIN), i64::from(i32::MAX)))
                .unwrap_or_default();
            match s[l] {
                b'>' => o.level = n,
                b'e' => o.open_imag = n,
                b'n' => o.no_brace = n,
                b'f' => o.first_open = n,
                b'{' => o.open_extra = n,
                b'}' => o.close_extra = n,
                b'^' => o.open_left_imag = n,
                b'L' => o.jump_label = n,
                b':' => o.case = n,
                b'=' => o.case_code = n,
                b'b' => o.case_break = n,
                b'p' => o.param = n,
                b't' => o.func_type = n,
                b'/' => o.comment = n,
                b'c' => o.in_comment = n,
                b'C' => o.in_comment2 = n,
                b'i' => o.cpp_baseclass = n,
                b'+' => o.continuation = n,
                b'(' => o.unclosed = n,
                b'u' => o.unclosed2 = n,
                b'U' => o.unclosed_noignore = n,
                b'W' => o.unclosed_wrapped = n,
                b'w' => o.unclosed_whiteok = n,
                b'm' => o.matching_paren = n,
                b'M' => o.paren_prev = n,
                b')' => o.maxparen = n,
                b'*' => o.maxcomment = n,
                b'g' => o.scopedecl = n,
                b'h' => o.scopedecl_code = n,
                b'j' => o.java = n,
                b'J' => o.js = n,
                b'l' => o.keep_case_label = n,
                b'#' => o.hash_comment = n,
                b'N' => o.cpp_namespace = n,
                b'k' => o.if_for_while = n,
                b'E' => o.cpp_extern_c = n,
                b'P' => o.pragma = n,
                _ => {}
            }
            if at(s, p) == b',' {
                p += 1;
            }
        }
        o
    }
}

/// nvim's `getdigits`: a decimal number (an optional sign, then digits) at `*p`, or 0 when there is
/// none; `*p` moves past it.
pub(super) fn getdigits(s: &[u8], p: &mut usize) -> i64 {
    let start = *p;
    let mut i = *p;
    let neg = match at(s, i) {
        b'-' => {
            i += 1;
            true
        }
        b'+' => {
            i += 1;
            false
        }
        _ => false,
    };
    let ds = i;
    let mut n: i64 = 0;
    while at(s, i).is_ascii_digit() {
        n = n
            .saturating_mul(10)
            .saturating_add(i64::from(at(s, i) - b'0'));
        i += 1;
    }
    if i == ds {
        *p = start;
        return 0;
    }
    *p = i;
    if neg {
        -n
    } else {
        n
    }
}

/// C's `atoi` at `p`: the leading digits as a number, 0 when there are none.
fn atoi(s: &[u8], p: usize) -> i64 {
    let mut q = p;
    getdigits(s, &mut q)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_cinoptions() {
        let o = CinOptions::parse(">2s,e-1s,n.5s,:0,(0,L0", 4);
        assert_eq!(o.level, 8);
        assert_eq!(o.open_imag, -4);
        assert_eq!(o.no_brace, 2);
        assert_eq!(o.case, 0);
        assert_eq!(o.unclosed, 0);
        assert_eq!(o.jump_label, 0);
        let d = CinOptions::parse("", 4);
        assert_eq!(
            (d.level, d.unclosed, d.maxparen, d.maxcomment),
            (4, 8, 20, 70)
        );
    }
}
