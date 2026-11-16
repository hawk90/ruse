//! The fold Ex commands (#1141, #1500): `:foldopen`/`:foldclose`, `:folddoopen`/`:folddoclosed`
//! and `:fold`.

use ruse_core::SubRange;

use crate::app::session::{Editing, FrameCtx, Session, E350};

impl Session {
    /// `:{range}foldopen[!]` / `:{range}foldclose[!]` (#1141): `zo`/`zc` (`zO`/`zC`) on each line of
    /// the range, past each closed fold; `E490` when no fold holds any of its lines.
    pub(in crate::app::session) fn ex_fold_open(
        &mut self,
        range: SubRange,
        open: bool,
        recursive: bool,
    ) {
        let Session {
            ed: Editing { ws, status, .. },
            folds,
            ..
        } = self;
        let (a, b) = ws.resolve_range(range);
        let fv = folds.entry(ws.focused_buffer()).or_default();
        if !crate::folds::op_range(fv, a.max(1) - 1, b.max(1) - 1, open, recursive) {
            status.error("E490: No fold found");
        }
    }

    /// `:{range}folddoopen {cmd}` / `:{range}folddoclosed {cmd}` (#1141): run `{cmd}` on each line of
    /// the range not in (or in) a closed fold, through the per-line dispatch `:g` uses (nvim's
    /// `ex_folddo` marks the lines, then runs `global_exe`).
    pub(in crate::app::session) fn ex_fold_do(
        &mut self,
        range: SubRange,
        closed: bool,
        cmd: String,
        frame: &FrameCtx,
    ) {
        let Session {
            ed: Editing { ws, .. },
            folds,
            ..
        } = self;
        let (a, b) = ws.resolve_range(range);
        let fv = crate::folds::for_buffer(folds, ws.focused_buffer());
        let marked: Vec<usize> = (a.max(1)..=b)
            .filter(|&l| crate::folds::closed_range(fv, l - 1).is_some() == closed)
            .collect();
        if !marked.is_empty() {
            self.ex_global_ex(&marked, &cmd, frame);
        }
    }

    /// `:{range}fold` (#1141): a closed manual fold over the range, as `zf`; the cursor stays.
    pub(in crate::app::session) fn ex_fold(&mut self, range: SubRange) {
        let Session {
            ed: Editing { ws, status, .. },
            folds,
            fold_method,
            ..
        } = self;
        let buf = ws.focused_buffer();
        if fold_method.get(&buf).copied() == Some(crate::input::FoldMethod::Indent) {
            status.error(E350);
            return;
        }
        let (a, b) = ws.resolve_range(range);
        let fv = folds.entry(buf).or_default();
        crate::folds::insert_closed_fold(fv, a.max(1) - 1, b.max(1) - 1);
    }
}
