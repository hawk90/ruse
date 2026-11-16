//! `foldmethod=indent`: folds derived from indentation, and the `foldlevel` rule.

use super::Fold;

// ── `foldmethod=indent` (slice 3): folds are DERIVED from indentation, not created by hand. ───────────────
// These pure functions compute the nested fold set from the buffer bytes + shiftwidth, and translate a
// `foldlevel` value into per-fold closed state. The session recomputes on edit and re-applies `foldlevel`.

/// The indent LEVEL of a single line: its indentation in columns (tabs expanded to the `tabstop`) divided by
/// the `shiftwidth`, rounded DOWN (the caller resolves Vim's `shiftwidth=0` → `tabstop` fallback first).
/// nvim-verified: 4-space indent at sw=4 → 1, 8-space → 2, a 2-space indent at sw=4 → 0 (floor); a single
/// tab at ts=8/sw=4 → 8 cols → level 2.
#[must_use]
fn line_indent_level(line: &[u8], tabstop: impl Into<ruse_core::Tabs>, shiftwidth: usize) -> usize {
    let tabstop: ruse_core::Tabs = tabstop.into();
    let mut cols = 0usize;
    for &c in line {
        match c {
            b' ' => cols += 1,
            b'\t' => cols += tabstop.padding(cols),
            _ => break,
        }
    }
    cols / shiftwidth.max(1)
}

/// Whether a line is IGNORED for indent folding: it is blank (empty or all whitespace) or a `foldignore`
/// line. Vim's default `foldignore` is `#`, so a line whose first non-blank byte is `#` is ignored too
/// (nvim-verified: a `#`-leading line takes the surrounding fold level, exactly like a blank). An ignored
/// line's effective level is the level of the next NON-ignored line below (Vim's classic blank-line rule):
/// a blank/comment between two indented lines stays inside the fold, but a trailing blank before a dedent
/// (or at end of buffer) drops to level 0 and is excluded from the fold above.
fn is_ignored_line(line: &[u8]) -> bool {
    let first = line.iter().find(|&&c| c != b' ' && c != b'\t');
    matches!(first, None | Some(b'#'))
}

/// Compute the nested `foldmethod=indent` folds for `text` at the given `tabstop` and `shiftwidth`. Pure and
/// unit-tested against nvim v0.12.4. Every returned fold is OPEN (`closed: false`); the caller applies the
/// foldlevel rule via [`apply_foldlevel`].
///
/// The algorithm assigns each line an EFFECTIVE level (a non-ignored line's own indent level; an ignored
/// blank/`#` line takes the level of the next non-ignored line below, or 0 at trailing blanks / EOF). Then,
/// for each depth `d ≥ 1`, every MAXIMAL run of consecutive lines with effective level `≥ d` spanning `≥ 2`
/// lines becomes a fold whose `level` is the MINIMUM effective level within the run. So a 0→2 indent jump
/// yields a single level-2 fold (matching nvim), not a redundant level-1 wrapper, and a lone indented line
/// forms no fold (nvim: `foldclosed` is -1 for it). Identical `[start, end]` ranges are de-duplicated (the
/// jump case emits the same run at several depths). Line indices are 0-based and match the frontend's line
/// numbering (split on `\n`; a trailing `\n` does not add a phantom final line).
#[must_use]
pub fn compute_indent_folds(
    text: &[u8],
    tabstop: impl Into<ruse_core::Tabs>,
    shiftwidth: usize,
) -> Vec<Fold> {
    let tabstop: ruse_core::Tabs = tabstop.into();
    let mut lines: Vec<&[u8]> = text.split(|&b| b == b'\n').collect();
    // A trailing newline is the last real line's terminator, not a new empty line ("a\nb\n" = 2 lines).
    if lines.len() > 1 && lines.last() == Some(&&b""[..]) {
        lines.pop();
    }
    let n = lines.len();
    // Own indent level for non-ignored lines; `None` marks an ignored (blank/`#`) line, resolved next.
    let own: Vec<Option<usize>> = lines
        .iter()
        .map(|l| (!is_ignored_line(l)).then(|| line_indent_level(l, tabstop, shiftwidth)))
        .collect();
    // Effective level: scan bottom-up so an ignored line inherits the next non-ignored line's level below it.
    let mut eff = vec![0usize; n];
    let mut next_lvl = 0usize;
    for i in (0..n).rev() {
        match own[i] {
            Some(l) => {
                eff[i] = l;
                next_lvl = l;
            }
            None => eff[i] = next_lvl,
        }
    }
    let max_eff = eff.iter().copied().max().unwrap_or(0);
    let mut folds: Vec<Fold> = Vec::new();
    for d in 1..=max_eff {
        let mut i = 0;
        while i < n {
            if eff[i] < d {
                i += 1;
                continue;
            }
            let s = i;
            while i < n && eff[i] >= d {
                i += 1;
            }
            let e = i - 1;
            if e > s {
                let level = eff[s..=e].iter().copied().min().unwrap_or(d);
                if !folds.iter().any(|f| f.start == s && f.end == e) {
                    folds.push(Fold {
                        start: s,
                        end: e,
                        closed: false,
                        level,
                    });
                }
            }
        }
    }
    folds
}

/// Apply Vim's `foldlevel` rule to a set of (indent) folds: a fold is CLOSED iff its `level` EXCEEDS
/// `foldlevel`. `foldlevel = 0` closes every outermost fold; a value ≥ the deepest level opens all.
/// nvim-verified against `foldclosed()` at several `foldlevel` values on a nested buffer.
pub fn apply_foldlevel(folds: &mut [Fold], foldlevel: usize) {
    for f in folds.iter_mut() {
        f.closed = f.level > foldlevel;
    }
}

/// The deepest fold `level` present (0 if there are no folds) — the `foldlevel` value `zR` sets to open all.
#[must_use]
pub fn max_level(folds: &[Fold]) -> usize {
    folds.iter().map(|f| f.level).max().unwrap_or(0)
}

#[cfg(test)]
mod tests;
