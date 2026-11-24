//! `:retab` (#609): the command ([`EditorState::retab`]) and its per-line rewrite ([`retab_line`]), as
//! nvim's `ex_retab` does them.

use super::line_start_of;
use crate::edit::{Edit, EditList};
use crate::editor::{line_spans, range, resolve_line_range, EditorState, IndentStyle, SubRange};
use crate::editor::{Tabs, VarStops};

impl EditorState {
    /// `:[range]retab[!] [new_tabstop]` (#609), as nvim's `ex_retab` does it (v0.12.6): every run of white
    /// space that holds a `<Tab>` (with `bang`, also every run of two or more spaces) is rewritten for the
    /// same width — measured with the current `'tabstop'` — as tabs and spaces under `new_tabstop`, or as
    /// spaces only when `'expandtab'` is set. A run is left alone when the rewrite would not make it
    /// shorter, except under `'expandtab'` or when it holds a tab. The new stops are one width (a
    /// `'tabstop'`) or a list (a `'vartabstop'`, #1163); `None` keeps the stops in force. Afterwards
    /// `'vartabstop'` takes the new list when it was in use or the list has more than one stop, else
    /// `'tabstop'` takes the one width. The changed lines are one undo group, and the cursor stays on
    /// its line at the wanted column. Returns whether any line changed.
    pub fn retab(&mut self, range: SubRange, bang: bool, new_tabstop: Option<VarStops>) -> bool {
        let old = self.view.indent.tabs();
        let new = match new_tabstop.filter(|v| !v.is_empty()) {
            Some(v) => Tabs::new(old.tabstop(), v),
            None => old,
        };
        let expand = self.view.indent.style == IndentStyle::Space;
        let bytes = self.doc.bytes().to_vec();
        let hay: &[u8] = &bytes;
        let spans = line_spans(hay);
        let set_new = |st: &mut Self| {
            if let Some(v) = new_tabstop.filter(|v| !v.is_empty()) {
                if !st.view.indent.vts.is_empty() || v.count() > 1 {
                    st.view.indent.vts = v;
                } else {
                    st.view.indent.tab_width = usize::from(v.stops()[0]);
                }
            }
        };
        if spans.is_empty() {
            set_new(self);
            return false;
        }
        let cursor_line = crate::pos::raw::line_of(&bytes, self.view.cursor);
        // The wanted column the cursor returns to, measured with the tabstop in force now (Vim computes
        // it when it is needed, so a `:set ts` since the last move counts).
        if self.view.curswant != range::MAXCOL {
            self.view.curswant = self.wanted_col();
        }
        let (first, last) = resolve_line_range(range, &spans, cursor_line);
        let last = last.min(spans.len() - 1);
        let first = first.min(last);
        let mut out = bytes[..spans[first].0].to_vec();
        let mut first_changed = None;
        for li in first..=last {
            let (ls, le) = spans[li];
            let new_line = retab_line(&bytes[ls..le], bang, expand, old, new);
            if new_line != bytes[ls..le] {
                first_changed.get_or_insert(li);
            }
            out.extend_from_slice(&new_line);
            if li < last {
                out.extend_from_slice(&bytes[le..spans[li + 1].0]);
            }
        }
        out.extend_from_slice(&bytes[spans[last].1..]);
        set_new(self);
        let Some(first_changed) = first_changed else {
            return false;
        };
        let list = EditList::new(vec![Edit::lines_changed(self.doc.bytes(), out)])
            .expect("a single edit is well-formed");
        let group = self.doc.history().current_group();
        self.apply_ex_edit(list, self.origin, None)
            .expect("retab applies cleanly");
        self.view.last_was_edit = true;
        // `'.` at the first line it changed; `'[`/`']` stay (nvim's `ex_retab`, #1303). Retab keeps lines.
        self.set_ex_dot(self.line_pos(first_changed), Some(group));
        let nb = self.doc.bytes();
        let ls = line_start_of(nb, cursor_line).min(nb.len());
        self.view.cursor = self.relocated_cursor(nb, ls);
        self.settle_offsets();
        true
    }
}

/// One line of [`EditorState::retab`] (nvim's `ex_retab` loop): columns are measured with the `old` stops,
/// and a rewritten run is laid out with the `new` ones (`tabstop_fromto`). The line is walked by cluster with the cells
/// the renderer paints ([`range::cluster_cells`]: `<ff>` for a byte that is not UTF-8 is 4, `^A` 2), so a
/// line that is not UTF-8 is retabbed too (#1252).
fn retab_line(line: &[u8], bang: bool, expand: bool, old_ts: Tabs, new_ts: Tabs) -> Vec<u8> {
    let mut out = Vec::with_capacity(line.len());
    let (mut vcol, mut start_vcol, mut start) = (0usize, 0usize, 0usize);
    let (mut got_tab, mut num_spaces) = (false, 0usize);
    let mut copied = 0usize;
    let mut col = 0usize;
    loop {
        let c = line.get(col).copied();
        if matches!(c, Some(b' ' | b'\t')) {
            if !got_tab && num_spaces == 0 {
                start_vcol = vcol;
                start = col;
            }
            if c == Some(b' ') {
                num_spaces += 1;
            } else {
                got_tab = true;
            }
        } else {
            if got_tab || (bang && num_spaces > 1) {
                let len = vcol - start_vcol;
                let (mut tabs, mut spaces) = (0, len);
                if !expand {
                    (tabs, spaces) = new_ts.fromto(start_vcol, vcol);
                }
                if expand || got_tab || tabs + spaces < len {
                    out.extend_from_slice(&line[copied..start]);
                    out.extend(std::iter::repeat_n(b'\t', tabs));
                    out.extend(std::iter::repeat_n(b' ', spaces));
                    copied = col;
                }
            }
            got_tab = false;
            num_spaces = 0;
        }
        if c.is_none() {
            break;
        }
        let (next, w) = range::cluster_cells(line, col, line.len(), vcol, old_ts);
        vcol += w;
        col = next;
    }
    out.extend_from_slice(&line[copied..]);
    out
}
