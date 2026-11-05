//! How a message too long for the status line is shown (#1222). ruse's last row holds both the status bar
//! and the message after it, so a long message would be cut at the right edge. nvim v0.12.6 (probed in a
//! PTY at 80x24) shows such a message whole: it wraps across as many rows as it needs, the screen scrolls up,
//! and `Press ENTER or type command to continue` waits on the last row (`:help hit-enter`). A file message
//! (`"name" 3L, 12B written`) is cut at its start with a `<` instead, as the default `'shortmess'` flag `t`
//! does.

use unicode_segmentation::UnicodeSegmentation;

use crate::screen::{cluster_width, transcribe};

/// nvim's hit-enter prompt (`:help hit-enter`).
pub(crate) const HIT_ENTER: &str = "Press ENTER or type command to continue";

/// How the status bar shows its message.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Shown {
    /// The bar as it is: the message fits after the bar's prefix.
    Fits,
    /// A file message cut at its start (`<` and its tail), to put after the prefix.
    Cut(String),
    /// The message whole, wrapped to the screen width, one entry per row, for the rows above the
    /// hit-enter prompt.
    More(Vec<String>),
}

/// The cells `g` takes on the screen, a control char as its `^X` transcription ([`transcribe`]).
fn cells(g: &str) -> usize {
    match transcribe(g, false) {
        Some(t) => t
            .graphemes(true)
            .map(|c| usize::from(cluster_width(c)))
            .sum(),
        None => usize::from(cluster_width(g)),
    }
}

/// The cells `s` takes on one row.
pub(crate) fn width(s: &str) -> usize {
    s.graphemes(true).map(cells).sum()
}

/// How `message` is shown after a bar prefix `prefix_cells` wide on a screen `cols` wide.
pub(crate) fn fit(prefix_cells: usize, message: &str, cols: usize) -> Shown {
    let room = cols.saturating_sub(prefix_cells);
    if !message.contains('\n') && width(message) <= room {
        return Shown::Fits;
    }
    if is_file_message(message) && !message.contains('\n') && room > 1 {
        return Shown::Cut(cut_start(message, room));
    }
    Shown::More(wrap(message, cols))
}

/// A file message (`"f.txt" 3L, 12B written`, `"new.txt" [New]`): the quoted name, then a count, a `[`
/// flag or a `--` note. A quoted name followed by anything else (`"f.txt" not opened: …`) is not one.
fn is_file_message(message: &str) -> bool {
    let Some(rest) = message.strip_prefix('"') else {
        return false;
    };
    let Some(end) = rest.find('"') else {
        return false;
    };
    let after = rest[end + 1..].trim_start_matches(' ');
    after.starts_with(|c: char| c.is_ascii_digit() || c == '[') || after.starts_with("--")
}

/// `<` and as much of the end of `message` as fits in `room` cells with it.
fn cut_start(message: &str, room: usize) -> String {
    let mut tail = Vec::new();
    let mut used = 1;
    for g in message.graphemes(true).rev() {
        let w = cells(g);
        if used + w > room {
            break;
        }
        used += w;
        tail.push(g);
    }
    tail.reverse();
    format!("<{}", tail.concat())
}

/// `message` broken into rows of at most `cols` cells, at any cell as nvim does (not at word ends), and at
/// each newline.
fn wrap(message: &str, cols: usize) -> Vec<String> {
    let cols = cols.max(1);
    let mut rows = Vec::new();
    for line in message.split('\n') {
        let mut row = String::new();
        let mut used = 0;
        for g in line.graphemes(true) {
            let w = cells(g);
            if used + w > cols && used > 0 {
                rows.push(std::mem::take(&mut row));
                used = 0;
            }
            row.push_str(g);
            used += w;
        }
        rows.push(row);
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_message_that_fits_is_left_alone() {
        assert_eq!(fit(20, "E37: No write since last change", 80), Shown::Fits);
        assert_eq!(fit(0, &"x".repeat(80), 80), Shown::Fits);
    }

    #[test]
    fn a_long_message_wraps_at_the_screen_width() {
        let msg = format!("E303: {}", "y".repeat(100));
        let Shown::More(rows) = fit(20, &msg, 80) else {
            panic!("not wrapped");
        };
        assert_eq!(rows.len(), 2);
        assert_eq!(width(&rows[0]), 80);
        assert_eq!(rows.concat(), msg);
    }

    #[test]
    fn a_message_too_long_after_the_prefix_waits_for_enter_too() {
        // 60 cells fit the screen but not after a 30-cell prefix.
        let msg = "W".repeat(60);
        assert_eq!(fit(30, &msg, 80), Shown::More(vec![msg.clone()]));
    }

    #[test]
    fn a_newline_starts_a_row() {
        assert_eq!(
            fit(0, "one\ntwo", 80),
            Shown::More(vec!["one".into(), "two".into()])
        );
    }

    #[test]
    fn a_file_message_is_cut_at_its_start() {
        let msg = format!("\"{}/f.txt\" 3L, 12B written", "d".repeat(80));
        let Shown::Cut(shown) = fit(20, &msg, 80) else {
            panic!("not cut");
        };
        assert_eq!(width(&shown), 60);
        assert!(shown.starts_with("<ddd"), "{shown}");
        assert!(shown.ends_with("/f.txt\" 3L, 12B written"), "{shown}");
    }

    #[test]
    fn only_a_file_message_is_cut() {
        assert!(is_file_message("\"f.txt\" 3L, 12B written"));
        assert!(is_file_message("\"f.txt\" [New]"));
        assert!(is_file_message("\"f.txt\" --No lines in buffer--"));
        let refused = format!(
            "\"f.txt\" not opened: recovery journal kept ({})",
            "j".repeat(80)
        );
        assert!(!is_file_message(&refused));
        assert!(matches!(fit(20, &refused, 80), Shown::More(_)));
    }

    #[test]
    fn wide_chars_do_not_split_across_rows() {
        let Shown::More(rows) = fit(0, &"界".repeat(41), 81) else {
            panic!("not wrapped");
        };
        assert_eq!(rows.iter().map(|r| width(r)).collect::<Vec<_>>(), [80, 2]);
    }
}
