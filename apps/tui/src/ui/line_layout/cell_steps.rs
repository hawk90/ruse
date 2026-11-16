//! The per-cell steps of [`layout_from`](super::layout_from): one per cell kind (a concealed cluster, a
//! tab, an unprintable char, a text grapheme), the continuation-row heads and spans they push, and the
//! final `eol` cell and row count. Each step borrows the [`LineLayout`] being built mutably and the
//! [`Walk`] facts shared.

use ruse_core::cells::Glyph;
use unicode_segmentation::UnicodeSegmentation;

use super::{plain_size, shown_as, Cell, CellKind, CellText, CharSpan, LineLayout, Walk};
use crate::screen::cluster_width;

impl<'t> LineLayout<'t> {
    /// The step for a concealed cluster at byte `b` (`len` bytes) at `vcol`: it takes no cells, but the
    /// first one of a range (`replacement` is `Some`) paints the range's replacement text. Returns the
    /// virtual column after it.
    pub(super) fn push_concealed(
        &mut self,
        mut vcol: usize,
        b: usize,
        len: usize,
        replacement: Option<Option<&'t str>>,
    ) -> usize {
        if let Some(virt) = replacement {
            for vg in virt.unwrap_or("").graphemes(true) {
                let cw = usize::from(cluster_width(vg));
                self.cells.push(Cell {
                    vcol,
                    text: CellText::Str(vg),
                    width: cw,
                    byte: None,
                    kind: CellKind::Text,
                });
                vcol += cw;
            }
        }
        self.chars.push(CharSpan {
            byte: b,
            len,
            vcol,
            head: 0,
            size: 0,
            is_tab: false,
        });
        vcol
    }

    /// The step for the shown char `gi` at `vcol`: its span and its cells, by kind (a tab, an unprintable
    /// char, or text). Returns its size.
    pub(super) fn push_char(&mut self, w: &Walk<'t, '_>, gi: usize, vcol: usize) -> usize {
        let (b, g) = w.graphemes[gi];
        let is_tab = g == Glyph::Text("\t");
        let (size, head, mb_added) = w.size(gi, vcol);
        self.chars.push(CharSpan {
            byte: b,
            len: g.len(),
            vcol,
            head,
            size,
            is_tab,
        });
        if is_tab {
            self.push_tab(w, g, b, vcol, size, head);
        } else if let Some(shown) = shown_as(g, w.lay.uhex) {
            self.push_shown(w, &shown, b, vcol, size);
        } else if let Glyph::Text(g) = g {
            self.push_text(w, g, b, vcol, size, mb_added);
        }
        size
    }

    /// The cells of a tab: its own cells take the `'listchars'` fill; a `'linebreak'` padding after them
    /// stays blank, except after a one-cell tab, whose fill runs on through it (nvim's `win_line`).
    fn push_tab(
        &mut self,
        w: &Walk<'t, '_>,
        g: Glyph<'t>,
        b: usize,
        vcol: usize,
        size: usize,
        head: usize,
    ) {
        let plain = plain_size(g, vcol, w.ts, w.list_tab, false, false);
        let content = if plain == 1 {
            size - head
        } else {
            plain.min(size - head)
        };
        let kind = if w.lay.list.is_some() {
            CellKind::Whitespace
        } else {
            CellKind::Text
        };
        self.push_span(w, vcol, size, b, |k| {
            if k < content {
                (w.tab_cell(k, content), 1, kind)
            } else {
                (CellText::Blank, 1, CellKind::Text)
            }
        });
    }

    /// The cells of an unprintable char: it shows transcribed, one cell per char (`^[`, `<85>`), as nvim's
    /// SpecialKey: never the raw control byte (#1181); a byte that is not UTF-8 shows as `<ff>` (#1251).
    fn push_shown(&mut self, w: &Walk<'t, '_>, shown: &str, b: usize, vcol: usize, size: usize) {
        let cells: Vec<char> = shown.chars().collect();
        self.push_span(w, vcol, size, b, |k| match cells.get(k) {
            Some(&c) if crate::screen::cluster_width(c.encode_utf8(&mut [0; 4])) == 1 => {
                (CellText::Char(c), 1, CellKind::Whitespace)
            }
            _ => (CellText::Blank, 1, CellKind::Text),
        });
    }

    /// The cells of a text grapheme: the `>` filler of a wide glyph that did not fit (`mb_added`), the
    /// glyph, and blanks for the rest of its size.
    fn push_text(
        &mut self,
        w: &Walk<'t, '_>,
        g: &'t str,
        b: usize,
        vcol: usize,
        size: usize,
        mb_added: usize,
    ) {
        let (text, kind) = w.glyph(b, g);
        let gw = usize::from(cluster_width(g)).max(1);
        self.push_span(w, vcol, size, b, |k| {
            if k < mb_added {
                (CellText::Char('>'), 1, CellKind::NonText)
            } else if k == mb_added {
                (text.clone(), gw, kind)
            } else {
                (CellText::Blank, 1, CellKind::Text)
            }
        });
    }

    /// The last step, at `vcol` after the last char walked: the `total`, the `'listchars'` `eol` cell
    /// (not when the walk was `truncated`), and the `rows`.
    pub(super) fn finish(&mut self, w: &Walk<'t, '_>, vcol: usize, truncated: bool) {
        self.total = vcol;
        let mut end = vcol;
        if let Some(eol) = w.lay.list.and_then(|l| l.eol).filter(|_| !truncated) {
            let mut at = vcol;
            if self.starts_row(at) && w.head_w > 0 {
                self.push_head(w, at);
                at += w.head_w;
            }
            self.cells.push(Cell {
                vcol: at,
                text: CellText::Char(eol),
                width: 1,
                byte: Some(w.bytes.len()),
                kind: CellKind::NonText,
            });
            end = at + 1;
        }
        self.rows = if w.lay.wrap {
            end.div_ceil(self.width).max(1)
        } else {
            1
        };
    }

    /// Whether virtual column `vcol` starts a continuation row.
    fn starts_row(&self, vcol: usize) -> bool {
        self.wrap && vcol >= self.width && vcol.is_multiple_of(self.width)
    }

    /// Push the head of the continuation row that starts at `vcol`: the `'breakindent'` indent and the
    /// `'showbreak'` text, the latter first under `briopt` `sbr`.
    fn push_head(&mut self, w: &Walk<'t, '_>, vcol: usize) {
        if !self.with_cells {
            return;
        }
        let sbr_first = w.lay.breakindent.is_some_and(|o| o.sbr);
        let mut at = vcol;
        if !sbr_first {
            at += w.bri;
        }
        for sg in w.sbr.graphemes(true) {
            let cw = usize::from(cluster_width(sg));
            if at + cw > vcol + w.head_w {
                break;
            }
            self.cells.push(Cell {
                vcol: at,
                text: if sg == "\t" {
                    CellText::Blank
                } else {
                    CellText::Str(sg)
                },
                width: cw,
                byte: None,
                kind: CellKind::NonText,
            });
            at += cw;
        }
    }

    /// Push the cells of the char at byte `byte` spanning `[vcol, vcol + size)`: the head of each
    /// continuation row it starts or crosses, and its content cells, the `k`th of which `content(k)`
    /// names (a glyph wider than one cell covers the next ones).
    fn push_span(
        &mut self,
        w: &Walk<'t, '_>,
        vcol: usize,
        size: usize,
        byte: usize,
        mut content: impl FnMut(usize) -> (CellText<'t>, usize, CellKind),
    ) {
        if !self.with_cells {
            return;
        }
        let mut p = vcol;
        let mut k = 0;
        while p < vcol + size {
            if w.head_w > 0 && self.starts_row(p) {
                self.push_head(w, p);
                p += w.head_w;
                continue;
            }
            let (text, cw, kind) = content(k);
            self.cells.push(Cell {
                vcol: p,
                text,
                width: cw,
                byte: Some(byte),
                kind,
            });
            p += cw.max(1);
            k += cw.max(1);
        }
    }
}
