//! Frontend-resolved motions (#615): the screen-line motions (`gj`, `gk`, `g0`, `g^`, `gm`, `g$`) and the
//! viewport motions `H`/`M`/`L`. The engine hands them out with their folded count and the frontend
//! finishes them through the methods here. Moved out of `input/mod.rs` (#1493).

use ruse_core::Motion;

use super::{Feed, InputEngine};

/// A screen-line motion (`:help gj`): it moves over screen rows, not buffer lines (#615).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScreenMotion {
    /// `gj` / `g<Down>`.
    Down,
    /// `gk` / `g<Up>`.
    Up,
    /// `g0` / `g<Home>`.
    Start,
    /// `g^`.
    FirstNonBlank,
    /// `gm`.
    Middle,
    /// `g$` / `g<End>`.
    End,
}

impl InputEngine {
    /// The EFFECTIVE count for a viewport screen-motion (`H`/`M`/`L`), WITHOUT consuming any state. When an
    /// operator is armed this is the operator count times the motion count (`3d2H` = 6H → the 6th line from
    /// the top, verified against nvim), the SAME multiplication [`motion`](Self::motion) applies; with no
    /// operator it is the raw pending count (0 = none, which the viewport resolver treats as 1). A frontend
    /// intercept resolves its target line from this, then hands the line to [`screen_op`](Self::screen_op).
    pub fn screen_count(&self) -> u32 {
        if self.normal.op.is_some() {
            self.mcount()
        } else {
            self.normal.typed()
        }
    }

    /// Whether an operator (`d`/`c`/`y`/`>`/`=`/`gu`…) is currently armed, WITHOUT consuming it. The `H`/`M`/
    /// `L` frontend intercept peeks this to pick the target line's SCROLLOFF: the bare cursor-motion keeps a
    /// `'scrolloff'` margin from the window edge, but under an operator Vim IGNORES scrolloff — `dH`/`dL`
    /// operate through the TRUE top/bottom visible line (verified against nvim). So the intercept resolves the
    /// line with `scrolloff = 0` when this is true, and the normal margin otherwise.
    pub fn has_op(&self) -> bool {
        self.normal.op.is_some()
    }

    /// The OPERATOR form of the viewport-resolved screen motions `H`/`M`/`L` (`dH`/`yL`/`c2M`/`>H`/`gUL`…):
    /// compose the pending operator (if any) over `GotoLine` to the ABSOLUTE 1-based `line` the frontend
    /// resolved from the viewport, then clear the transient state. These motions are LINEWISE under an
    /// operator (`:help H`), which the core `GotoLine` operator path already gives — the range is `[min,max]`
    /// of the cursor and target lines, so direction does not matter. With NO operator armed this is the bare
    /// cursor move (`Move(line, GotoLine)`), preserving plain `H`/`M`/`L`. The frontend owns the viewport, so
    /// it resolves `line` (honoring any `[count]`, read via [`peek_count`](Self::peek_count)) and passes it in.
    pub fn screen_op(&mut self, line: u32) -> Feed {
        self.op_over_motion(Motion::GotoLine, line)
    }

    /// Hand a screen-line motion to the frontend ([`Feed::ScreenMotion`]), keeping the operator armed.
    pub(super) fn screen_motion(&mut self, motion: ScreenMotion) -> Feed {
        Feed::ScreenMotion {
            motion,
            count: self.mcount(),
        }
    }

    /// Finish a [`Feed::ScreenMotion`] the frontend resolved to buffer byte `byte`: the bare move, or the
    /// armed operator over it as an exclusive charwise motion (`go`'s rules: `dgj` from a line's start is
    /// linewise). An inclusive one (`g$`) reaches here as the byte after its char.
    pub fn screen_motion_to(&mut self, byte: usize) -> Feed {
        let target = u32::try_from(byte.saturating_add(1)).unwrap_or(u32::MAX);
        self.op_over_motion(Motion::GotoByte, target)
    }

    /// Finish a [`Feed::ScreenMotion`] that kept the end-of-line wanted column (`gj` after `$`) and landed
    /// on the end of a line `lines - 1` below as `{lines}$`: the same place, inclusive, and the column
    /// stays at the end as nvim keeps it.
    pub fn screen_motion_line_end(&mut self, lines: u32) -> Feed {
        self.op_over_motion(Motion::LineEnd, lines.max(1))
    }

    /// Finish a [`Feed::ScreenMotion`] as `j` / `k` with its count: `gj` / `gk` without 'wrap'.
    pub fn screen_motion_lines(&mut self, down: bool) -> Feed {
        let total = self.mcount();
        self.op_over_motion(if down { Motion::Down } else { Motion::Up }, total)
    }

    /// Abandon a [`Feed::ScreenMotion`] that cannot move (`gj` on the last screen row): nvim beeps and
    /// drops the operator.
    pub fn screen_motion_fail(&mut self) -> Feed {
        let had_register = self.normal.register;
        self.reset();
        self.drop_aborted_register(
            had_register,
            Feed::Beep {
                clear_register: false,
            },
        )
    }
}
