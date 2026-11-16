//! The fold family: `'foldmethod'`, `'foldlevel'` and `'foldenable'`, frontend state per buffer.

use ruse_core::DocumentId;

use super::OptionHost;
use crate::input::set_cmd::{Opt, OptVal};
use crate::input::FoldMethod;

impl OptionHost<'_> {
    /// The value of a fold option for `buf`.
    pub(super) fn get_fold(&self, opt: Opt, buf: DocumentId) -> OptVal {
        match opt {
            Opt::FoldEnable => OptVal::Bool(self.foldenable.get(&buf).copied().unwrap_or(true)),
            Opt::FoldLevel => OptVal::Num(
                i64::try_from(self.foldlevel.get(&buf).copied().unwrap_or(0)).unwrap_or(i64::MAX),
            ),
            Opt::FoldMethod => OptVal::Str(match self.fold_method.get(&buf) {
                Some(FoldMethod::Indent) => "indent",
                _ => "manual",
            }),
            _ => unreachable!(),
        }
    }

    /// The fold options (frontend state, per buffer). Switching the method keeps the folds and
    /// recomputes them next frame (Vim keeps them); a level or enable change re-applies at once.
    pub(super) fn put_fold(&mut self, opt: Opt, val: OptVal, on: bool, buf: DocumentId) {
        match (opt, val) {
            (Opt::FoldMethod, OptVal::Str(m)) => {
                let m = if m == "indent" {
                    FoldMethod::Indent
                } else {
                    FoldMethod::Manual
                };
                // Folds an indent method made stay as manual ones, at their depths.
                if m == FoldMethod::Manual {
                    if let Some(fv) = self.folds.get_mut(&buf) {
                        crate::folds::renest(fv);
                    }
                }
                self.fold_method.insert(buf, m);
                self.fold_rev.remove(&buf);
            }
            (Opt::FoldLevel, OptVal::Num(n)) => {
                self.foldlevel.insert(buf, usize::try_from(n).unwrap_or(0));
                self.reapply_folds_for(buf, true);
            }
            (Opt::FoldEnable, _) => {
                self.foldenable.insert(buf, on);
                self.reapply_folds(buf);
            }
            _ => {}
        }
    }

    /// Close the folds deeper than 'foldlevel' (all of them open with 'nofoldenable'). Manual folds take a
    /// new 'foldlevel' too (nvim's `newFoldLevel`), but not a 'foldenable' change, which would lose the
    /// state each one was left in.
    fn reapply_folds(&mut self, buf: DocumentId) {
        self.reapply_folds_for(buf, false);
    }

    fn reapply_folds_for(&mut self, buf: DocumentId, level_changed: bool) {
        if self.fold_method.get(&buf).copied() != Some(FoldMethod::Indent) && !level_changed {
            return;
        }
        if let Some(fv) = self.folds.get_mut(&buf) {
            let level = if self.foldenable.get(&buf).copied().unwrap_or(true) {
                self.foldlevel.get(&buf).copied().unwrap_or(0)
            } else {
                usize::MAX
            };
            crate::folds::apply_foldlevel(fv, level);
        }
    }
}
