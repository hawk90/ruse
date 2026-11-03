//! The viewport — a frontend-only view concern (the core stays view-free, INV-DOC-VIEW). A window's
//! vertical scroll is one `top` offset (its first displayed buffer line); [`update_topline`] keeps the
//! cursor on screen counting wrapped rows (#615). See `docs/design/render-and-frontends.md` (v0 section).

/// The lines a window scrolls over, as nvim's `update_topline` sees them (#615): how many there are,
/// the screen rows each takes (wrapped rows, plus any virtual rows below it), and the closed folds.
/// Rows are 0-based buffer lines.
pub trait Lines {
    /// The number of buffer lines (at least 1).
    fn count(&self) -> usize;
    /// The screen rows line `l` takes, at least 1 (nvim's `plines_win_nofill`, unlimited).
    fn height(&self, l: usize) -> usize;
    /// The closed fold holding line `l`, as `(first, last)`.
    fn fold(&self, l: usize) -> Option<(usize, usize)>;
    /// Whether the window has any closed fold (nvim's `hasAnyFolding`).
    fn any_fold(&self) -> bool;
}

/// A line's rows, or 1 for a closed fold, limited to the window `height` (nvim's
/// `plines_win_nofill(.., true)`).
fn rows_of(lines: &impl Lines, l: usize, height: usize) -> usize {
    if lines.fold(l).is_some() {
        1
    } else {
        lines.height(l).clamp(1, height.max(1))
    }
}

/// nvim's `lineoff_T`: a line and its height while stepping over the buffer (`MAXCOL` past an end).
#[derive(Clone, Copy)]
struct LineOff {
    lnum: isize,
    height: usize,
}

const MAXCOL: usize = usize::MAX / 4;

fn topline_back(lines: &impl Lines, lp: &mut LineOff, height: usize) {
    lp.lnum -= 1;
    if lp.lnum < 0 {
        lp.height = MAXCOL;
    } else if let Some((s, _)) = lines.fold(lp.lnum as usize) {
        lp.lnum = s as isize;
        lp.height = 1;
    } else {
        lp.height = rows_of(lines, lp.lnum as usize, height);
    }
}

fn botline_forw(lines: &impl Lines, lp: &mut LineOff, height: usize) {
    lp.lnum += 1;
    if lp.lnum as usize >= lines.count() {
        lp.height = MAXCOL;
    } else if let Some((_, e)) = lines.fold(lp.lnum as usize) {
        lp.lnum = e as isize;
        lp.height = 1;
    } else {
        lp.height = rows_of(lines, lp.lnum as usize, height);
    }
}

/// The first line below the window whose rows do not all fit when `top` is the first line (nvim's
/// `w_botline`, `count` when the buffer ends inside the window), and the rows left unused below the
/// last line that fits (`w_empty_rows`).
pub fn botline(lines: &impl Lines, top: usize, height: usize) -> (usize, usize) {
    let mut done = 0;
    let mut l = top;
    while l < lines.count() {
        let (n, last) = match lines.fold(l) {
            Some((_, e)) => (1, e),
            None => (rows_of(lines, l, height), l),
        };
        if done + n > height {
            break;
        }
        done += n;
        l = last + 1;
    }
    (l, if done == 0 { 0 } else { height - done })
}

/// The first line of the closed fold holding `l`, else `l`.
fn fold_start(lines: &impl Lines, l: usize) -> usize {
    lines.fold(l).map_or(l, |(s, _)| s)
}

/// nvim's `check_top_offset`: whether fewer than 'scrolloff' screen rows show above the cursor line.
fn check_top_offset(
    lines: &impl Lines,
    top: usize,
    cursor: usize,
    so: usize,
    height: usize,
) -> bool {
    if cursor < top + so || lines.any_fold() {
        let mut loff = LineOff {
            lnum: cursor as isize,
            height: 0,
        };
        let mut n = 0;
        while n < so {
            topline_back(lines, &mut loff, height);
            if loff.lnum < top as isize {
                break;
            }
            n += loff.height;
        }
        if n < so {
            return true;
        }
    }
    false
}

/// nvim's `scroll_cursor_halfway` (without 'smoothscroll'): the top that puts the cursor line about in
/// the middle of the window, adding a line below and one above it in turn (the one above first when
/// `prefer_above`) until the window is full.
fn halfway(lines: &impl Lines, cursor: usize, height: usize, prefer_above: bool) -> usize {
    let (lo, hi) = lines.fold(cursor).unwrap_or((cursor, cursor));
    let mut loff = LineOff {
        lnum: lo as isize,
        height: 0,
    };
    let mut boff = LineOff {
        lnum: hi as isize,
        height: 0,
    };
    let mut used = rows_of(lines, lo, height);
    let mut topline = lo;
    'outer: while topline > 0 {
        let (mut above, mut below) = (0usize, 0usize);
        for round in 1..=2 {
            let add_below = if prefer_above {
                round == 2 && below < above
            } else {
                round == 1 && below <= above
            };
            if add_below {
                if (boff.lnum as usize) + 1 < lines.count() {
                    botline_forw(lines, &mut boff, height);
                    used = used.saturating_add(boff.height);
                    if used > height {
                        break 'outer;
                    }
                    below += boff.height;
                } else {
                    below += 1; // a `~` row
                }
            }
            let add_above = if prefer_above {
                round == 1 && below >= above
            } else {
                round == 1 && below > above
            };
            if add_above {
                topline_back(lines, &mut loff, height);
                used = used.saturating_add(loff.height);
                if used > height {
                    break 'outer;
                }
                above += loff.height;
                topline = loff.lnum as usize;
            }
        }
    }
    fold_start(lines, topline)
}

/// nvim's `scroll_cursor_top`: scroll up so the cursor has 'scrolloff' rows above it, at least
/// `min_scroll` ('scrolljump') rows when it scrolls at all.
fn scroll_cursor_top(
    lines: &impl Lines,
    top: usize,
    cursor: usize,
    height: usize,
    so: usize,
    min_scroll: usize,
) -> usize {
    let mut scrolled = 0;
    let mut used = rows_of(lines, cursor, height);
    if cursor < top {
        scrolled = used;
    }
    let (mut t, mut b) = match lines.fold(cursor) {
        Some((s, e)) => (s as isize - 1, e as isize + 1),
        None => (cursor as isize - 1, cursor as isize + 1),
    };
    let mut new_top = t + 1;
    let mut extra = 0;
    while t >= 0 {
        let i = match lines.fold(t as usize) {
            Some((s, _)) => {
                t = s as isize;
                1
            }
            None => rows_of(lines, t as usize, height),
        };
        if (t as usize) < top {
            scrolled += i;
        }
        if (new_top >= top as isize || scrolled > min_scroll) && extra >= so {
            break;
        }
        used += i;
        if extra + i <= so && b + 1 < lines.count() as isize {
            match lines.fold(b as usize) {
                Some((_, e)) => {
                    b = e as isize;
                    used += 1;
                }
                None => used += rows_of(lines, b as usize, height),
            }
        }
        if used > height {
            break;
        }
        extra += i;
        new_top = t;
        t -= 1;
        b += 1;
    }
    if used > height {
        return halfway(lines, cursor, height, false);
    }
    let mut top = top;
    if (new_top as usize) < top {
        top = new_top as usize;
    }
    top.min(cursor)
}

/// nvim's `scroll_cursor_bot`: scroll down so the cursor line shows whole with 'scrolloff' rows below
/// it, at least `min_scroll` rows when it scrolls at all.
fn scroll_cursor_bot(
    lines: &impl Lines,
    top: usize,
    cursor: usize,
    height: usize,
    so: usize,
    min_scroll: usize,
) -> usize {
    let (botline, empty_rows) = botline(lines, top, height);
    let mut used = rows_of(lines, cursor, height);
    let mut scrolled: isize = 0;
    if cursor >= botline {
        scrolled = used as isize;
        if cursor == botline {
            scrolled -= empty_rows as isize;
        }
    }
    let (lo, hi) = lines.fold(cursor).unwrap_or((cursor, cursor));
    let mut loff = LineOff {
        lnum: lo as isize,
        height: 0,
    };
    let mut boff = LineOff {
        lnum: hi as isize,
        height: 0,
    };
    let mut extra = 0usize;
    let count = lines.count() as isize;
    while loff.lnum > 0 {
        let enough = (scrolled <= 0 || scrolled >= min_scroll as isize) && extra >= so;
        if (enough || boff.lnum + 1 >= count) && loff.lnum <= botline as isize {
            break;
        }
        let loff_before = loff.lnum;
        topline_back(lines, &mut loff, height);
        used = used.saturating_add(loff.height);
        if used > height {
            break;
        }
        if loff.lnum >= botline as isize {
            scrolled += loff.height as isize;
            if loff.lnum == botline as isize && loff_before > botline as isize {
                scrolled -= empty_rows as isize;
            }
        }
        if boff.lnum + 1 < count {
            let boff_before = boff.lnum;
            botline_forw(lines, &mut boff, height);
            used = used.saturating_add(boff.height);
            if used > height {
                break;
            }
            if extra < so || scrolled < min_scroll as isize {
                extra += boff.height;
                if boff.lnum >= botline as isize {
                    scrolled += boff.height as isize;
                    if boff_before < botline as isize {
                        scrolled -= empty_rows as isize;
                    }
                }
            }
        }
    }
    let line_count = if scrolled <= 0 {
        0
    } else if used > height {
        used
    } else {
        let mut n = 0;
        let mut i = 0isize;
        let mut b = LineOff {
            lnum: top as isize - 1,
            height: 0,
        };
        while i < scrolled && b.lnum < botline as isize {
            botline_forw(lines, &mut b, height);
            i += b.height as isize;
            n += 1;
        }
        if i < scrolled {
            9999
        } else {
            n
        }
    };
    if line_count >= height && line_count > min_scroll {
        halfway(lines, cursor, height, true)
    } else if line_count > 0 {
        scroll_down_lines(lines, top, line_count)
    } else {
        top
    }
}

/// The top `n` lines further down, a closed fold counting as one (nvim's `scrollup`).
fn scroll_down_lines(lines: &impl Lines, top: usize, n: usize) -> usize {
    let mut t = top;
    for _ in 0..n {
        let next = lines.fold(t).map_or(t, |(_, e)| e) + 1;
        if next >= lines.count() {
            break;
        }
        t = next;
    }
    t
}

/// 'scrolljump' as rows: a negative value is a percentage of the window height (nvim's
/// `scrolljump_value`).
#[must_use]
pub fn scrolljump_rows(sj: i64, height: usize) -> usize {
    if sj >= 0 {
        usize::try_from(sj).unwrap_or(usize::MAX)
    } else {
        height * usize::try_from(-sj).unwrap_or(0) / 100
    }
}

/// The first displayed line after the cursor moved: nvim v0.12.6's `update_topline` without
/// 'smoothscroll' (#615). A cursor above the window, or with fewer than `so` rows above it, scrolls the
/// window up; one below it, or with fewer than `so` rows below it, scrolls it down; a jump further than
/// about half a window puts the cursor line in the middle. Rows count wrapped lines and closed folds
/// count one row, so the cursor line always shows whole when it fits.
#[must_use]
pub fn update_topline(
    lines: &impl Lines,
    top: usize,
    cursor: usize,
    height: usize,
    so: usize,
    min_scroll: usize,
) -> usize {
    update_topline_sms(lines, top, cursor, height, so, min_scroll, None).0
}

/// nvim's `scroll_cursor_halfway` with 'smoothscroll' (#615): the cursor line in the middle of the
/// window to the row, the line above it cut by skipped columns (`width` cells a row). Returns the top
/// line and its skipped columns.
fn halfway_sms(lines: &impl Lines, cursor: usize, height: usize, width: usize) -> (usize, usize) {
    let (lo, hi) = lines.fold(cursor).unwrap_or((cursor, cursor));
    let mut loff = LineOff {
        lnum: lo as isize,
        height: 0,
    };
    let mut boff = LineOff {
        lnum: hi as isize,
        height: 0,
    };
    let mut used = rows_of(lines, lo, height);
    let mut topline = lo;
    let mut skipcol = 0;
    while topline > 0 {
        // The line above at its full height.
        loff.lnum -= 1;
        if loff.lnum < 0 {
            break;
        }
        loff.height = match lines.fold(loff.lnum as usize) {
            Some((s, _)) => {
                loff.lnum = s as isize;
                1
            }
            None => lines.height(loff.lnum as usize).max(1),
        };
        used += loff.height;
        if (boff.lnum as usize) + 1 < lines.count() {
            botline_forw(lines, &mut boff, height);
            used += boff.height;
        }
        if used > height {
            if used - loff.height < height {
                topline = loff.lnum as usize;
                // nvim's `skipcol_from_plines`.
                let off = used - height;
                skipcol = width * off;
            }
            break;
        }
        topline = loff.lnum as usize;
    }
    (fold_start(lines, topline), skipcol)
}

/// [`update_topline`] with 'smoothscroll' when `sms` holds the text width: a far jump then centres the
/// cursor to the row (the top line's skipped columns come back as the second value, `None` when the
/// top line starts whole as before).
#[must_use]
pub fn update_topline_sms(
    lines: &impl Lines,
    top: usize,
    cursor: usize,
    height: usize,
    so: usize,
    min_scroll: usize,
    sms: Option<usize>,
) -> (usize, Option<usize>) {
    let half = |prefer_above: bool| match sms {
        Some(w) => {
            let (t, s) = halfway_sms(lines, cursor, height, w);
            (t, Some(s))
        }
        None => (halfway(lines, cursor, height, prefer_above), None),
    };
    let count = lines.count().max(1);
    let cursor = cursor.min(count - 1);
    if height == 0 {
        return (cursor, None);
    }
    let mut top = top.min(count - 1);
    let check_topline =
        top > 0 && (cursor < top || check_top_offset(lines, top, cursor, so, height));
    if check_topline {
        let halfheight = (height as isize / 2 - 1).max(2);
        let n: isize = if lines.any_fold() {
            let mut n = 0;
            let mut l = cursor;
            while l < top + so {
                n += 1;
                if l + 1 >= count || n >= halfheight {
                    break;
                }
                l = lines.fold(l).map_or(l, |(_, e)| e) + 1;
            }
            n
        } else {
            (top + so) as isize - cursor as isize
        };
        if n >= halfheight {
            // Far above: centred, and nvim checks nothing below.
            return half(false);
        }
        top = scroll_cursor_top(lines, top, cursor, height, so, min_scroll);
    } else {
        top = fold_start(lines, top);
    }
    let (botline, empty_rows) = botline(lines, top, height);
    if botline < count {
        let mut need = true;
        if cursor < botline {
            if cursor + so >= botline || lines.any_fold() {
                // Count the window rows below the cursor line.
                let mut n = empty_rows;
                let mut loff = LineOff {
                    lnum: lines.fold(cursor).map_or(cursor, |(_, e)| e) as isize,
                    height: 0,
                };
                while loff.lnum < botline as isize {
                    n = n.saturating_add(loff.height);
                    if n >= so {
                        break;
                    }
                    botline_forw(lines, &mut loff, height);
                }
                need = n < so;
            } else {
                need = false;
            }
        }
        if need {
            let line_count: isize = if lines.any_fold() {
                let mut n = 0isize;
                let mut l = cursor as isize;
                while l >= botline as isize - so as isize {
                    n += 1;
                    if l <= 0 || n > height as isize + 1 {
                        break;
                    }
                    l = fold_start(lines, l as usize) as isize - 1;
                }
                n
            } else {
                cursor as isize - botline as isize + 1 + so as isize
            };
            if line_count <= height as isize + 1 {
                top = scroll_cursor_bot(lines, top, cursor, height, so, min_scroll);
            } else {
                return half(false);
            }
        }
    }
    (top, None)
}

/// Where a recenter command (`z`) places the cursor's line in the window.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RecenterTo {
    /// `zz` / `z.` — the cursor's line at the vertical center.
    Center,
    /// `zt` / `z<CR>` — the cursor's line at the top.
    Top,
    /// `zb` / `z-` — the cursor's line at the bottom.
    Bottom,
}

/// The scroll offset (`top`) that places `cursor_row` at the requested position in a `height`-row window
/// (Vim `z`). Pure and saturating. The per-frame [`scroll_top`] pass then applies `scrolloff`, so `Top`/
/// `Bottom` end up `scrolloff` rows inside the edge exactly as Vim's `zt`/`zb` do; `Center` is unaffected.
pub fn recenter(cursor_row: usize, height: usize, to: RecenterTo) -> usize {
    match to {
        RecenterTo::Top => cursor_row,
        RecenterTo::Center => cursor_row.saturating_sub(height / 2),
        RecenterTo::Bottom => cursor_row.saturating_sub(height.saturating_sub(1)),
    }
}

/// Which visible line `H`/`M`/`L` target.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ScreenTo {
    /// `H` — top of the window (`{count}H` = count lines below the top), scrolloff-aware.
    High,
    /// `M` — the middle visible line.
    Middle,
    /// `L` — bottom of the window (`{count}L` = count lines above the bottom), scrolloff-aware.
    Low,
}

/// The buffer row `H`/`M`/`L` move the cursor to, given the window's `top` / `height` and `scrolloff`,
/// a `count` (0 = none), and the buffer's `last_line`. Pure; the row is clamped into the visible range.
/// `H`/`L` keep a `scrolloff` margin from the edge unless the edge is the buffer start/end (as in Vim).
pub fn screen_line(
    top: usize,
    height: usize,
    scrolloff: usize,
    to: ScreenTo,
    count: u32,
    last_line: usize,
) -> usize {
    let bottom = (top + height.saturating_sub(1)).min(last_line);
    let n = (count.max(1) - 1) as usize; // extra lines from a count
    let row = match to {
        ScreenTo::High => {
            let floor = if top > 0 { scrolloff } else { 0 };
            top + floor.max(n)
        }
        ScreenTo::Middle => top + (bottom - top) / 2,
        ScreenTo::Low => {
            let floor = if bottom < last_line { scrolloff } else { 0 };
            bottom.saturating_sub(floor.max(n))
        }
    };
    row.clamp(top, bottom)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Lines of the given heights, with closed folds.
    struct Rows(Vec<usize>, Vec<(usize, usize)>);

    impl Lines for Rows {
        fn count(&self) -> usize {
            self.0.len()
        }
        fn height(&self, l: usize) -> usize {
            self.0[l]
        }
        fn fold(&self, l: usize) -> Option<(usize, usize)> {
            self.1.iter().copied().find(|&(s, e)| l >= s && l <= e)
        }
        fn any_fold(&self) -> bool {
            !self.1.is_empty()
        }
    }

    /// Follow `steps` of `(cursor, expected top)` (1-based, as nvim reports them) from the top.
    fn follow(lines: &Rows, height: usize, so: usize, steps: &[(usize, usize)]) {
        let mut top = 0;
        for &(cursor, want) in steps {
            top = update_topline(lines, top, cursor - 1, height, so, 1);
            assert_eq!(top + 1, want, "cursor {cursor}, so {so}");
        }
    }

    /// nvim v0.12.6 headless, 100 one-row lines, a 23-row window, `scrolloff=3`: a far jump centres the
    /// cursor line, a near one scrolls just enough.
    #[test]
    fn update_topline_matches_nvim_on_jumps() {
        let lines = Rows(vec![1; 100], vec![]);
        follow(
            &lines,
            23,
            3,
            &[
                (50, 39),
                (20, 9),
                (45, 34),
                (46, 34),
                (75, 64),
                (60, 57),
                (30, 19),
                (29, 19),
            ],
        );
    }

    /// nvim v0.12.6 headless, 40 lines in an 8-row window with line 10 wrapped over 4 rows.
    #[test]
    fn update_topline_counts_wrapped_rows_like_nvim() {
        let mut h = vec![1; 40];
        h[9] = 4;
        let lines = Rows(h, vec![]);
        follow(
            &lines,
            8,
            0,
            &[
                (5, 1),
                (7, 1),
                (8, 1),
                (9, 2),
                (10, 6),
                (11, 7),
                (12, 8),
                (8, 8),
                (6, 6),
            ],
        );
        follow(
            &lines,
            8,
            3,
            &[
                (1, 1),
                (4, 1),
                (5, 1),
                (6, 2),
                (7, 2),
                (8, 6),
                (9, 6),
                (10, 8),
                (13, 11),
                (14, 11),
                (13, 11),
                (12, 10),
                (11, 10),
                (10, 8),
                (9, 7),
                (8, 6),
                (40, 33),
            ],
        );
    }

    #[test]
    fn screen_line_targets_are_scrolloff_aware() {
        // Window rows 10..=29 (top=10, height=20), scrolloff 3, big buffer.
        assert_eq!(screen_line(10, 20, 3, ScreenTo::High, 1, 999), 13); // top + scrolloff
        assert_eq!(screen_line(10, 20, 3, ScreenTo::Low, 1, 999), 26); // bottom(29) - scrolloff
        assert_eq!(screen_line(10, 20, 3, ScreenTo::Middle, 1, 999), 19); // top + 19/2
        assert_eq!(screen_line(10, 20, 3, ScreenTo::High, 5, 999), 14); // 5H = 4 below top (> scrolloff)
                                                                        // At the buffer top, H has no scrolloff floor.
        assert_eq!(screen_line(0, 20, 3, ScreenTo::High, 1, 999), 0);
        // A short buffer clamps the bottom to the last line.
        assert_eq!(screen_line(0, 20, 3, ScreenTo::Low, 1, 5), 5);
    }

    /// Under an OPERATOR, `H`/`L` IGNORE 'scrolloff' and target the TRUE top/bottom visible line, while the
    /// bare cursor-motion keeps the scrolloff margin — so the frontend resolves the operator target with
    /// `scrolloff = 0`. All targets here are ground truth captured from nvim v0.12.4 (`set lines`, `zt`).
    #[test]
    fn operator_screen_targets_ignore_scrolloff() {
        // nvim: 12-line buffer, window rows 1..=10 at the buffer top (0-based top=0, last_line=11).
        // Operator (scrolloff=0): dH -> line 1 (row 0), dL -> line 10 (row 9), dM -> line 5 (row 4).
        assert_eq!(screen_line(0, 10, 0, ScreenTo::High, 1, 11), 0);
        assert_eq!(screen_line(0, 10, 0, ScreenTo::Low, 1, 11), 9);
        assert_eq!(screen_line(0, 10, 0, ScreenTo::Middle, 1, 11), 4);
        // Counts still count from the edge: d2H -> line 2 (row 1), d2L -> line 9 (row 8).
        assert_eq!(screen_line(0, 10, 0, ScreenTo::High, 2, 11), 1);
        assert_eq!(screen_line(0, 10, 0, ScreenTo::Low, 2, 11), 8);
        // nvim: 40-line buffer scrolled so window rows 19..=28 (0-based top=18, last_line=39).
        // Operator ignores scrolloff: dH -> line 19 (row 18), dL -> line 28 (row 27).
        assert_eq!(screen_line(18, 10, 0, ScreenTo::High, 1, 39), 18);
        assert_eq!(screen_line(18, 10, 0, ScreenTo::Low, 1, 39), 27);
        // The bare cursor-motion (scrolloff=3) DOES keep the margin: H -> line 22 (row 21), L -> line 25 (24).
        assert_eq!(screen_line(18, 10, 3, ScreenTo::High, 1, 39), 21);
        assert_eq!(screen_line(18, 10, 3, ScreenTo::Low, 1, 39), 24);
    }

    #[test]
    fn recenter_positions_the_cursor_line() {
        // A 20-row window, cursor at row 50.
        assert_eq!(recenter(50, 20, RecenterTo::Top), 50);
        assert_eq!(recenter(50, 20, RecenterTo::Center), 40); // 50 - 20/2
        assert_eq!(recenter(50, 20, RecenterTo::Bottom), 31); // 50 - (20-1)
                                                              // Near the top of the buffer, everything saturates at 0 (no negative scroll).
        assert_eq!(recenter(2, 20, RecenterTo::Center), 0);
        assert_eq!(recenter(0, 20, RecenterTo::Bottom), 0);
    }

    #[test]
    fn short_buffer_never_scrolls() {
        // Cursor anywhere in a buffer that fits leaves the top pinned at 0.
        let lines = Rows(vec![1; 24], vec![]);
        for row in 0..24 {
            assert_eq!(
                update_topline(&lines, 0, row, 24, 3, 1),
                0,
                "row {row} fits"
            );
        }
    }

    #[test]
    fn zero_height_is_total() {
        let lines = Rows(vec![1; 50], vec![]);
        assert_eq!(update_topline(&lines, 0, 42, 0, 3, 1), 42);
    }
}
