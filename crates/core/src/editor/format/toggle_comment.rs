//! `gc`: nvim's `toggle_lines` (`runtime/lua/vim/_comment.lua`): comment lines out with
//! `'commentstring'`'s parts ([`toggle_comment`]), or uncomment them one line at a time
//! ([`uncomment`]).

/// nvim's `toggle_lines` for `gc`: comment the lines out with `'commentstring'`'s parts, or uncomment
/// them when every non-blank one is commented. `None` when `'commentstring'` has no `%s`.
pub(crate) fn toggle_comment(lines: &[&[u8]], cms: &str) -> Option<Vec<Vec<u8>>> {
    let at = cms.find("%s")?;
    let left = &cms.as_bytes()[..at];
    let right = &cms.as_bytes()[at + 2..];
    let trim = |s: &[u8]| -> Vec<u8> {
        let a = s
            .iter()
            .position(|c| !c.is_ascii_whitespace())
            .unwrap_or(s.len());
        let b = s
            .iter()
            .rposition(|c| !c.is_ascii_whitespace())
            .map_or(a, |p| p + 1);
        s[a..b.max(a)].to_vec()
    };
    let (lt, rt) = (trim(left), trim(right));
    let is_blank = |l: &[u8]| l.iter().all(u8::is_ascii_whitespace);
    // A commented line: blanks, the trimmed left part, anything, the trimmed right part, blanks.
    let commented = |l: &[u8]| {
        let s = l
            .iter()
            .position(|c| !c.is_ascii_whitespace())
            .unwrap_or(l.len());
        let e = l
            .iter()
            .rposition(|c| !c.is_ascii_whitespace())
            .map_or(s, |p| p + 1);
        let body = &l[s..e.max(s)];
        body.len() >= lt.len() + rt.len() && body.starts_with(&lt) && body.ends_with(&rt)
    };
    let mut indent: Option<&[u8]> = None;
    let mut all = true;
    for l in lines {
        let w = l.iter().take_while(|c| c.is_ascii_whitespace()).count();
        if w < l.len() {
            if indent.is_none_or(|i| w < i.len()) {
                indent = Some(&l[..w]);
            }
            if all {
                all = commented(l);
            }
        }
    }
    let indent = indent.unwrap_or(&[]);
    let out = if all {
        lines
            .iter()
            .map(|l| uncomment(l, left, right, &lt, &rt))
            .collect()
    } else {
        lines
            .iter()
            .map(|l| {
                if is_blank(l) {
                    let mut v = indent.to_vec();
                    v.extend_from_slice(&lt);
                    v.extend_from_slice(&rt);
                    v
                } else {
                    let mut v = indent.to_vec();
                    v.extend_from_slice(left);
                    v.extend_from_slice(&l[indent.len().min(l.len())..]);
                    v.extend_from_slice(right);
                    v
                }
            })
            .collect()
    };
    Some(out)
}

/// One line uncommented the way `vim._comment` does (`^(%s*)L(.*)R(%s-)$` with the exact parts, then
/// with the trimmed ones); a line that matches neither stays.
fn uncomment(l: &[u8], left: &[u8], right: &[u8], lt: &[u8], rt: &[u8]) -> Vec<u8> {
    let ws = |c: &u8| c.is_ascii_whitespace();
    let find = |left: &[u8], right: &[u8]| -> Option<(usize, usize, usize, usize)> {
        let lead = l.iter().take_while(|c| ws(c)).count();
        for ind in (0..=lead).rev() {
            if !l[ind..].starts_with(left) {
                continue;
            }
            let bs = ind + left.len();
            for be in (bs..=l.len()).rev() {
                if l[be..].starts_with(right) && l[be + right.len()..].iter().all(ws) {
                    return Some((ind, bs, be, be + right.len()));
                }
            }
        }
        None
    };
    let Some((ind, bs, be, te)) = find(left, right).or_else(|| find(lt, rt)) else {
        return l.to_vec();
    };
    let body = &l[bs..be];
    if body.iter().all(ws) {
        return body.to_vec();
    }
    let mut v = l[..ind].to_vec();
    v.extend_from_slice(body);
    v.extend_from_slice(&l[te..]);
    v
}
