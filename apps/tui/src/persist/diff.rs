//! The recovery prompt's `d` view (RFC-0023 §7): a line-based unified diff between the disk text and the
//! recovered text, computed with Myers' O(ND) algorithm here rather than with a crate.
//!
//! The work is bounded. The common leading and trailing lines are cut first (cheap, and usually most of a
//! recovery). The search for a minimal diff of what is left stops at [`MAX_D`] differing lines or [`MAX_WORK`]
//! comparison steps, and is not tried at all on more than [`MAX_LINES`] lines. When it stops, the changed
//! block is shown whole (every disk line removed, every recovered line added): still a correct diff, only not
//! the smallest, and the view says so. The output is cut at [`MAX_OUTPUT`] lines, and the view says that too.

/// Lines of context around each change, as `diff -u`.
pub const CONTEXT: usize = 3;
/// The edit distance (lines removed plus lines added) the minimal search may reach.
pub const MAX_D: usize = 2000;
/// Comparison steps the minimal search may take.
pub const MAX_WORK: usize = 20_000_000;
/// Lines (disk plus recovered, after the common ends are cut) above which no minimal search is tried.
pub const MAX_LINES: usize = 1_000_000;
/// Output lines kept; the rest are counted, not shown.
pub const MAX_OUTPUT: usize = 100_000;

/// A unified diff, ready to show.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Diff {
    /// The diff's lines: the `---`/`+++` header, then the hunks. Bytes that are not UTF-8 are shown as
    /// U+FFFD; nothing is escaped here (the view does that).
    pub lines: Vec<String>,
    /// Whether the diff is the smallest one (the bounded search finished).
    pub minimal: bool,
    /// Lines not kept because of [`MAX_OUTPUT`].
    pub cut: usize,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Op {
    Keep,
    Del,
    Ins,
}

/// Split into lines, each with its `\n` (the last one may have none), so a missing final newline differs.
fn lines(b: &[u8]) -> Vec<&[u8]> {
    b.split_inclusive(|&c| c == b'\n').collect()
}

/// The shortest edit script from `a` to `b`, or `None` when the bounds stop the search.
fn myers(a: &[&[u8]], b: &[&[u8]]) -> Option<Vec<Op>> {
    let (n, m) = (a.len(), b.len());
    let max = n + m;
    if max == 0 {
        return Some(Vec::new());
    }
    // `v[k + off]` is the furthest x reached on diagonal k; `off` leaves room for k = -max-1 ..= max+1.
    let off = max as isize + 1;
    let mut v = vec![0u32; 2 * max + 3];
    // `trace[d]` is the window k = -d-1 ..= d+1 of `v` as round d began.
    let mut trace: Vec<Vec<u32>> = Vec::new();
    let mut work = 0usize;
    let at = |k: isize| (k + off) as usize;
    for d in 0..=max as isize {
        if d as usize > MAX_D {
            return None;
        }
        trace.push(v[at(-d - 1)..=at(d + 1)].to_vec());
        let mut k = -d;
        while k <= d {
            let mut x = if k == -d || (k != d && v[at(k - 1)] < v[at(k + 1)]) {
                v[at(k + 1)] as usize
            } else {
                v[at(k - 1)] as usize + 1
            };
            let mut y = (x as isize - k) as usize;
            while x < n && y < m && a[x] == b[y] {
                x += 1;
                y += 1;
                work += 1;
            }
            work += 1;
            if work > MAX_WORK {
                return None;
            }
            v[at(k)] = x as u32;
            if x >= n && y >= m {
                return Some(backtrack(&trace, n, m));
            }
            k += 2;
        }
    }
    None
}

/// Walk the trace back from (n, m) to (0, 0); the script in order.
fn backtrack(trace: &[Vec<u32>], n: usize, m: usize) -> Vec<Op> {
    let (mut x, mut y) = (n as isize, m as isize);
    let mut ops = Vec::with_capacity(n + m);
    for d in (0..trace.len() as isize).rev() {
        let w = &trace[d as usize];
        let get = |k: isize| w[(k + d + 1) as usize] as isize;
        let k = x - y;
        let prev_k = if k == -d || (k != d && get(k - 1) < get(k + 1)) {
            k + 1
        } else {
            k - 1
        };
        let prev_x = get(prev_k);
        let prev_y = prev_x - prev_k;
        while x > prev_x && y > prev_y {
            ops.push(Op::Keep);
            x -= 1;
            y -= 1;
        }
        if d > 0 {
            ops.push(if x == prev_x { Op::Ins } else { Op::Del });
        }
        x = prev_x;
        y = prev_y;
    }
    while x > 0 && y > 0 {
        ops.push(Op::Keep);
        x -= 1;
        y -= 1;
    }
    ops.reverse();
    ops
}

/// `-start,count` / `+start,count` of a hunk, as GNU `diff -u` writes it (`,1` left out; an empty side
/// names the line before it).
fn range(start: usize, count: usize) -> String {
    match count {
        0 => format!("{start},0"),
        1 => format!("{}", start + 1),
        _ => format!("{},{count}", start + 1),
    }
}

fn show(line: &[u8]) -> String {
    let body = line.strip_suffix(b"\n").unwrap_or(line);
    String::from_utf8_lossy(body).into_owned()
}

/// The unified diff from `disk` (`--- {name} (on disk)`) to `recovered` (`+++ {name} (recovered)`).
pub fn unified(name: &str, disk: &[u8], recovered: &[u8]) -> Diff {
    let a = lines(disk);
    let b = lines(recovered);
    let pre = a.iter().zip(&b).take_while(|(x, y)| x == y).count();
    let suf = a[pre..]
        .iter()
        .rev()
        .zip(b[pre..].iter().rev())
        .take_while(|(x, y)| x == y)
        .count();
    let (ma, mb) = (&a[pre..a.len() - suf], &b[pre..b.len() - suf]);
    let middle = if ma.len() + mb.len() > MAX_LINES {
        None
    } else {
        myers(ma, mb)
    };
    let minimal = middle.is_some();
    let middle = middle.unwrap_or_else(|| {
        let mut ops = vec![Op::Del; ma.len()];
        ops.extend(std::iter::repeat_n(Op::Ins, mb.len()));
        ops
    });
    let mut ops = vec![Op::Keep; pre];
    ops.extend(middle);
    ops.extend(std::iter::repeat_n(Op::Keep, suf));

    // Each op's position in a and in b.
    let mut pos = Vec::with_capacity(ops.len());
    let (mut i, mut j) = (0, 0);
    for op in &ops {
        pos.push((i, j));
        match op {
            Op::Keep => {
                i += 1;
                j += 1;
            }
            Op::Del => i += 1,
            Op::Ins => j += 1,
        }
    }
    let mut out = vec![
        format!("--- {name} (on disk)"),
        format!("+++ {name} (recovered)"),
    ];
    let mut cut = 0;
    let mut push = |out: &mut Vec<String>, s: String| {
        if out.len() < MAX_OUTPUT {
            out.push(s);
        } else {
            cut += 1;
        }
    };
    let changed: Vec<usize> = (0..ops.len()).filter(|&k| ops[k] != Op::Keep).collect();
    let mut c = 0;
    while c < changed.len() {
        // A hunk: changes no more than 2 * CONTEXT kept lines apart, with CONTEXT lines around.
        let mut last = changed[c];
        let mut e = c + 1;
        while e < changed.len() && changed[e] - last <= 2 * CONTEXT + 1 {
            last = changed[e];
            e += 1;
        }
        let from = changed[c].saturating_sub(CONTEXT);
        let to = (last + CONTEXT + 1).min(ops.len());
        let (ai, bj) = pos[from];
        let count = |want: Op| {
            ops[from..to]
                .iter()
                .filter(|&&o| o == Op::Keep || o == want)
                .count()
        };
        let (na, nb) = (count(Op::Del), count(Op::Ins));
        push(
            &mut out,
            format!("@@ -{} +{} @@", range(ai, na), range(bj, nb)),
        );
        for k in from..to {
            let (i, j) = pos[k];
            let (mark, line) = match ops[k] {
                Op::Keep => (' ', a[i]),
                Op::Del => ('-', a[i]),
                Op::Ins => ('+', b[j]),
            };
            push(&mut out, format!("{mark}{}", show(line)));
            if !line.ends_with(b"\n") {
                push(&mut out, "\\ No newline at end of file".to_string());
            }
        }
        c = e;
    }
    Diff {
        lines: out,
        minimal,
        cut,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(d: &Diff) -> Vec<&str> {
        d.lines[2..].iter().map(String::as_str).collect()
    }

    #[test]
    fn a_changed_line_gets_context_and_a_gnu_header() {
        let disk = b"1\n2\n3\n4\n5\n6\n7\n8\n";
        let rec = b"1\n2\n3\n4\nFIVE\n6\n7\n8\n";
        let d = unified("f.txt", disk, rec);
        assert!(d.minimal);
        assert_eq!(d.lines[0], "--- f.txt (on disk)");
        assert_eq!(d.lines[1], "+++ f.txt (recovered)");
        assert_eq!(
            body(&d),
            [
                "@@ -2,7 +2,7 @@",
                " 2",
                " 3",
                " 4",
                "-5",
                "+FIVE",
                " 6",
                " 7",
                " 8"
            ]
        );
    }

    #[test]
    fn far_apart_changes_are_separate_hunks_and_near_ones_merge() {
        let numbered =
            |f: &dyn Fn(usize) -> String| -> String { (1..=30).map(|i| f(i) + "\n").collect() };
        let disk = numbered(&|i| i.to_string());
        let rec = numbered(&|i| match i {
            2 => "two".into(),
            29 => "X".into(),
            _ => i.to_string(),
        });
        let d = unified("f", disk.as_bytes(), rec.as_bytes());
        let hunks: Vec<_> = body(&d)
            .into_iter()
            .filter(|l| l.starts_with("@@"))
            .collect();
        assert_eq!(hunks, ["@@ -1,5 +1,5 @@", "@@ -26,5 +26,5 @@"]);
        let rec = numbered(&|i| match i {
            3 => "three".into(),
            9 => "nine".into(),
            _ => i.to_string(),
        });
        let d = unified("f", disk.as_bytes(), rec.as_bytes());
        let hunks = body(&d).into_iter().filter(|l| l.starts_with("@@")).count();
        assert_eq!(hunks, 1, "six kept lines between changes are one hunk");
    }

    #[test]
    fn insertions_into_empty_and_a_missing_final_newline() {
        let d = unified("f", b"", b"a\nb\n");
        assert_eq!(body(&d), ["@@ -0,0 +1,2 @@", "+a", "+b"]);
        let d = unified("f", b"a\n", b"a");
        assert_eq!(
            body(&d),
            ["@@ -1 +1 @@", "-a", "+a", "\\ No newline at end of file"]
        );
    }

    #[test]
    fn a_huge_difference_falls_back_to_the_whole_block() {
        let disk: String = (0..3000).map(|i| format!("d{i}\n")).collect();
        let rec: String = (0..3000).map(|i| format!("r{i}\n")).collect();
        let d = unified("f", disk.as_bytes(), rec.as_bytes());
        assert!(!d.minimal, "beyond MAX_D");
        assert_eq!(d.lines.len(), 2 + 1 + 6000);
        assert_eq!(d.lines[2], "@@ -1,3000 +1,3000 @@");
        assert_eq!(d.cut, 0);
    }

    /// Applying a diff's `-`/`+` lines in order gives back each side: the script is a real edit script.
    mod fuzz {
        use super::*;
        use proptest::prelude::*;

        fn sides(d: &Diff) -> (String, String) {
            let (mut a, mut b) = (String::new(), String::new());
            for l in &d.lines[2..] {
                match l.as_bytes().first() {
                    Some(b' ') => {
                        a += &l[1..];
                        a += "\n";
                        b += &l[1..];
                        b += "\n";
                    }
                    Some(b'-') => {
                        a += &l[1..];
                        a += "\n";
                    }
                    Some(b'+') => {
                        b += &l[1..];
                        b += "\n";
                    }
                    _ => {}
                }
            }
            (a, b)
        }

        proptest! {
            #![proptest_config(crate::test_cases(256))]

            #[test]
            fn myers_is_a_minimal_valid_script(
                a in proptest::collection::vec(0u8..4, 0..20),
                b in proptest::collection::vec(0u8..4, 0..20),
            ) {
                let al: Vec<Vec<u8>> = a.iter().map(|c| vec![b'a' + c, b'\n']).collect();
                let bl: Vec<Vec<u8>> = b.iter().map(|c| vec![b'a' + c, b'\n']).collect();
                let ar: Vec<&[u8]> = al.iter().map(Vec::as_slice).collect();
                let br: Vec<&[u8]> = bl.iter().map(Vec::as_slice).collect();
                let ops = myers(&ar, &br).unwrap();
                // The script turns a into b.
                let (mut i, mut j) = (0, 0);
                for op in &ops {
                    match op {
                        Op::Keep => { prop_assert_eq!(ar[i], br[j]); i += 1; j += 1; }
                        Op::Del => i += 1,
                        Op::Ins => j += 1,
                    }
                }
                prop_assert_eq!((i, j), (a.len(), b.len()));
                // Minimal: the kept lines are a longest common subsequence.
                let mut lcs = vec![vec![0usize; b.len() + 1]; a.len() + 1];
                for x in (0..a.len()).rev() {
                    for y in (0..b.len()).rev() {
                        lcs[x][y] = if a[x] == b[y] { lcs[x + 1][y + 1] + 1 } else { lcs[x + 1][y].max(lcs[x][y + 1]) };
                    }
                }
                prop_assert_eq!(ops.iter().filter(|&&o| o == Op::Keep).count(), lcs[0][0]);
            }

            #[test]
            fn the_hunks_cover_every_change(
                a in proptest::collection::vec(0u8..3, 0..40),
                b in proptest::collection::vec(0u8..3, 0..40),
            ) {
                let disk: String = a.iter().map(|c| format!("{c}\n")).collect();
                let rec: String = b.iter().map(|c| format!("{c}\n")).collect();
                let d = unified("f", disk.as_bytes(), rec.as_bytes());
                let (x, y) = sides(&d);
                // Every removed line comes from the disk and every added one from the recovery, in order.
                prop_assert!(is_subsequence(&x, &disk) && is_subsequence(&y, &rec));
                if disk == rec { prop_assert_eq!(d.lines.len(), 2); }
            }
        }

        fn is_subsequence(part: &str, whole: &str) -> bool {
            let mut it = whole.lines();
            part.lines().all(|l| it.any(|w| w == l))
        }
    }
}
