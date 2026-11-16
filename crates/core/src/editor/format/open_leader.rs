//! The comment leader a line opened from a comment takes: the leader half of nvim v0.12.6's
//! `open_line` (`change.c`, `OPENLINE_DO_COM`), split along its phases:
//!
//! 1. find the leader and read its part's flags ([`com_type`]: `get_leader_len` and the
//!    `for (p = lead_flags; ...)` loop over `COM_START` / `COM_MIDDLE` / `COM_END` / `COM_FIRST`);
//! 2. put the replacement leader in place ([`align_leader`]: the `COM_RIGHT` / `COM_LEFT` adjustment
//!    of `lead_repl` with its offset);
//! 3. shift the indent by the offset ([`shift_indent`]: the `if (newindent || did_si)` block), then add
//!    the `extra_space` blank.

use super::{leader_len, skipwhite, white, ComPart};
use crate::cells::Tabs;
use crate::editor::indent::indent_cols;

/// What a line opened from `saved` takes from its comment leader (the leader half of nvim's `open_line`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct OpenLeader {
    /// The leader text the new line starts with, blanks included (empty: no leader).
    pub(crate) leader: Vec<u8>,
    /// The indent the new line takes instead of `saved`'s, when the leader was replaced (a start or end
    /// part opens the middle one) under `'autoindent'` / `'smartindent'`.
    pub(crate) newindent: Option<usize>,
    /// The column in `saved` of a comment end (`*/`) that closes the comment: the new line lines up with
    /// the line that opened it.
    pub(crate) comment_end: Option<usize>,
}

/// The comment leader a line opened from `saved` takes (nvim's `open_line` with `OPENLINE_DO_COM`).
/// `saved` is the source line, cut at the cursor when the open splits it (`p_extra`: Insert `<CR>` or an
/// auto-wrap break, with `col` the cursor column); `forward` is `o` / a split, else `O`. `indent_on` is
/// `'autoindent'` or `'smartindent'`.
pub(crate) fn open_leader(
    saved: &[u8],
    parts: &[ComPart],
    forward: bool,
    split: Option<usize>,
    indent_on: bool,
    ts: Tabs,
) -> OpenLeader {
    let mut out = OpenLeader::default();
    let (lead_len, k) = leader_len(saved, parts, !forward, true);
    let Some(k) = k.filter(|_| lead_len > 0) else {
        return out;
    };
    let flags = parts[k].flags.as_bytes();
    let ct = com_type(saved, parts, k, lead_len, forward, split);
    out.comment_end = ct.comment_end;
    if ct.lead_len == 0 {
        return out;
    }
    let mut extra_space = ct.extra_space;
    let mut leader = saved[..ct.lead_len].to_vec();
    if let Some(repl) = ct.lead_repl {
        let (right, off) = align_flags(flags);
        leader = align_leader(&leader, &repl, right);
        if indent_on {
            out.newindent = Some(shift_indent(&mut leader, off, ts));
        }
    }
    if leader.last().copied().is_some_and(white) {
        extra_space = false;
    }
    if extra_space {
        leader.push(b' ');
    }
    out.leader = leader;
    out
}

/// What the flags of the leader's part make of it (phase 1 of `open_line`'s leader).
struct ComType {
    /// The leader bytes of `saved` kept (`lead_len`; 0: the new line takes no leader).
    lead_len: usize,
    /// The text that replaces the leader (`lead_repl`), when the part opens another one.
    lead_repl: Option<Vec<u8>>,
    /// A blank goes after the leader (`extra_space`).
    extra_space: bool,
    /// See [`OpenLeader::comment_end`].
    comment_end: Option<usize>,
}

/// Phase 1: the `lead_flags` loop of nvim's `open_line` over part `k`, the one matched by the
/// `lead_len`-byte leader at the start of `saved`.
fn com_type(
    saved: &[u8],
    parts: &[ComPart],
    k: usize,
    mut lead_len: usize,
    forward: bool,
    split: Option<usize>,
) -> ComType {
    let flags = parts[k].flags.as_bytes();
    let mut comment_end = None;
    let mut lead_repl: Option<Vec<u8>> = None;
    let mut extra_space = false;
    let mut require_blank = false;
    for &f in flags {
        match f {
            b'b' => require_blank = true,
            b's' | b'm' => {
                // The middle and end parts follow a start part, the end part follows a middle one.
                let (middle, end) = if f == b's' {
                    if !forward {
                        lead_len = 0;
                        break;
                    }
                    require_blank = false;
                    (parts.get(k + 1), parts.get(k + 2))
                } else {
                    (parts.get(k), parts.get(k + 1))
                };
                if f == b's' {
                    require_blank = middle.is_some_and(|m| m.has('b'));
                } else {
                    require_blank |= parts[k].has('b');
                }
                let lead_middle = middle.map(|m| m.text.clone()).unwrap_or_default();
                let lead_end = end.map(|e| e.text.clone()).unwrap_or_default();
                // The comment ends on this very line: no leader.
                if forward && !lead_end.is_empty() {
                    if let Some(p) =
                        (lead_len..saved.len()).find(|&p| saved[p..].starts_with(&lead_end))
                    {
                        comment_end = Some(p);
                        lead_len = 0;
                    }
                }
                if lead_len > 0 {
                    if f == b's' {
                        lead_repl = Some(lead_middle);
                    }
                    let typed_after = match split {
                        Some(col) => col == lead_len,
                        None => saved.len() == lead_len,
                    };
                    if !white(saved[lead_len - 1]) && (typed_after || require_blank) {
                        extra_space = true;
                    }
                }
                break;
            }
            b'e' => {
                if forward {
                    comment_end = Some(skipwhite(saved, 0));
                    lead_len = 0;
                    break;
                }
                // `O` on a comment end opens the middle part, the one before it.
                lead_repl = Some(
                    k.checked_sub(1)
                        .map(|m| parts[m].text.clone())
                        .unwrap_or_default(),
                );
                extra_space = true;
                break;
            }
            b'f' => {
                if forward {
                    lead_repl = Some(Vec::new());
                } else {
                    lead_len = 0;
                }
                break;
            }
            _ => {}
        }
    }
    ComType {
        lead_len,
        lead_repl,
        extra_space,
        comment_end,
    }
}

/// The alignment flags of a leader's part: `r` (right) / `l` (left, the default) and the last offset
/// number (`-` for a negative one), as nvim's `open_line` reads them from `lead_flags`.
fn align_flags(flags: &[u8]) -> (bool, i64) {
    let mut right = false;
    let mut off: i64 = 0;
    let mut p = 0;
    while p < flags.len() {
        match flags[p] {
            b'r' => {
                right = true;
                p += 1;
            }
            b'l' => {
                right = false;
                p += 1;
            }
            c if c.is_ascii_digit() || c == b'-' => {
                let neg = c == b'-';
                let mut q = p + usize::from(neg);
                let mut n: i64 = 0;
                while q < flags.len() && flags[q].is_ascii_digit() {
                    n = n * 10 + i64::from(flags[q] - b'0');
                    q += 1;
                }
                off = if neg { -n } else { n };
                p = q.max(p + 1);
            }
            _ => p += 1,
        }
    }
    (right, off)
}

/// Phase 2: `leader` with `repl` put in place of its comment text, right (`COM_RIGHT`) or left adjusted.
fn align_leader(leader: &[u8], repl: &[u8], right: bool) -> Vec<u8> {
    if right {
        // Line the replacement up with the old leader's last non-blank, blanking the rest.
        let mut endp = leader.len();
        while endp > 0 && white(leader[endp - 1]) {
            endp -= 1;
        }
        let start = endp.saturating_sub(repl.len());
        let mut new = vec![b' '; start];
        for (i, &c) in leader[..start].iter().enumerate() {
            if white(c) {
                new[i] = c;
            }
        }
        new.extend_from_slice(repl);
        new.extend_from_slice(&leader[endp..]);
        new
    } else {
        // Left adjusted: the replacement overwrites the leader's start, the rest becomes blanks (a
        // blank before a tab is dropped, so the indent stays the same).
        let p0 = skipwhite(leader, 0);
        let rest_len = leader.len() - p0;
        let mut i = 0;
        while i < rest_len && i < repl.len() {
            i += 1;
        }
        let mut new = leader[..p0].to_vec();
        new.extend_from_slice(repl);
        let tail = &leader[p0 + i.min(rest_len)..];
        let mut t = 0;
        while t < tail.len() {
            let c = tail[t];
            if white(c) {
                new.push(c);
            } else if tail.get(t + 1) != Some(&b'\t') {
                new.push(b' ');
            }
            t += 1;
        }
        new
    }
}

/// Phase 3: the indent the new line takes, the replaced `leader`'s shifted by `off` (not below 0), with
/// as many of the leader's trailing spaces dropped as the shift added, so the alignment stays.
fn shift_indent(leader: &mut Vec<u8>, mut off: i64, ts: Tabs) -> usize {
    let mut ni = indent_cols(leader, ts).0 as i64;
    if ni + off < 0 {
        off = -ni;
        ni = 0;
    } else {
        ni += off;
    }
    // Correct trailing spaces for the shift, so the alignment stays.
    let has_tab = leader[skipwhite(leader, 0)..].contains(&b'\t');
    while off > 0 && leader.last() == Some(&b' ') && !has_tab {
        leader.pop();
        off -= 1;
    }
    usize::try_from(ni).unwrap_or(0)
}
