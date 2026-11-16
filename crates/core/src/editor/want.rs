//! The wanted column (Vim `curswant`) a [`View`](super::View) keeps, measured lazily on first read
//! (#1231). Moved out of `editor/mod.rs` (#1495).

use super::range;
use crate::cells::Tabs;

/// The wanted column (Vim `curswant`), measured only when something reads it (#1231). Most commands
/// reset the column to the cursor's, but only vertical moves, block selections and a few edits ever read
/// it, and measuring walks the line from its start: on a multi-megabyte line that walk dominated every
/// keypress. As nvim (`w_set_curswant`), the column is set to "where the cursor is now" and measured on
/// first read. The pending form keeps what the measure reads (the buffer version, cursor, `'tabstop'` and
/// the tab-end rule), so it gives the value the eager measure would have given, whatever happens to the
/// buffer or the View before the read. Holding the buffer version is a refcount on the document's
/// `Arc`; it keeps at most one older version alive until the next command replaces the column.
#[derive(Clone)]
pub(crate) struct Want(pub(super) WantKind);

#[derive(Clone)]
pub(super) enum WantKind {
    /// A known column ([`range::MAXCOL`] after `$`).
    Col(usize),
    /// The column of `pos` in `text`, measured once on first read.
    At {
        text: std::sync::Arc<Vec<u8>>,
        pos: usize,
        ts: Tabs,
        tab_end: bool,
        col: std::sync::OnceLock<usize>,
    },
}

impl Want {
    pub(crate) const fn col(c: usize) -> Self {
        Want(WantKind::Col(c))
    }

    /// The column: measured now when pending ([`range::wanted_vcol`]), then remembered.
    pub(crate) fn get(&self) -> usize {
        match &self.0 {
            WantKind::Col(c) => *c,
            WantKind::At {
                text,
                pos,
                ts,
                tab_end,
                col,
            } => *col.get_or_init(|| range::wanted_vcol(text, *pos, *ts, *tab_end)),
        }
    }
}

/// `curswant == MAXCOL` and friends. A pending column is a measured position, never [`range::MAXCOL`], so
/// that test does not measure.
impl PartialEq<usize> for Want {
    fn eq(&self, other: &usize) -> bool {
        match &self.0 {
            WantKind::Col(c) => c == other,
            WantKind::At { .. } if *other == range::MAXCOL => false,
            WantKind::At { .. } => self.get() == *other,
        }
    }
}
