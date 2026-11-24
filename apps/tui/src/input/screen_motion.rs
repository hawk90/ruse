//! Frontend-resolved motions (#615): the screen-line motions (`gj`, `gk`, `g0`, `g^`, `gm`, `g$`) and the
//! viewport motions `H`/`M`/`L`. The engine hands them out with their folded count and the frontend
//! finishes them through the methods here. Moved out of `input/mod.rs` (#1493).

use ruse_core::{Mode, Motion};

use super::{repeat, ChangeIntent, Feed, InputEngine};

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

/// A [`Feed::ScreenMotion`] waiting for its finisher (#1539): the motion and its effective count, and
/// either how [`InputEngine::feed`] would have recorded the finished command or the change `.` repeats.
pub(super) struct ScreenHandoff {
    motion: ScreenMotion,
    count: u32,
    /// The mode the finished command is recorded in (`None`: it completes an `i_CTRL-O` one-shot).
    mode: Option<Mode>,
    /// The change `.` is repeating over this motion; its lead takes the span the frontend resolves.
    redo: Option<ChangeIntent>,
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
        // `.` repeating an operator over a screen-line motion has no operator armed, but resolves as one.
        self.normal.op.is_some()
            || self
                .screen_handoff
                .as_ref()
                .is_some_and(|h| h.redo.is_some())
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
        self.finish_screen_motion(Motion::GotoByte, target, false)
    }

    /// Finish a [`Feed::ScreenMotion`] that went BACK to buffer byte `byte` as an inclusive motion (`gk`
    /// after `$`, whose wanted column is the line's end; #1576): the armed operator spans from `byte`
    /// through the cursor's char, as nvim's inclusive end is the cursor, not the landing. The bare move
    /// is [`screen_motion_to`](Self::screen_motion_to)'s.
    pub fn screen_motion_back_inclusive(&mut self, byte: usize) -> Feed {
        let target = u32::try_from(byte.saturating_add(1)).unwrap_or(u32::MAX);
        self.finish_screen_motion(Motion::GotoByte, target, true)
    }

    /// Finish a [`Feed::ScreenMotion`] that kept the end-of-line wanted column (`gj` after `$`) and landed
    /// on the end of a line `lines - 1` below as `{lines}$`: the same place, inclusive, and the column
    /// stays at the end as nvim keeps it.
    pub fn screen_motion_line_end(&mut self, lines: u32) -> Feed {
        self.finish_screen_motion(Motion::LineEnd, lines.max(1), false)
    }

    /// Finish a [`Feed::ScreenMotion`] as `j` / `k` with its count: `gj` / `gk` without 'wrap'.
    pub fn screen_motion_lines(&mut self, down: bool) -> Feed {
        let total = self
            .screen_handoff
            .as_ref()
            .map_or_else(|| self.mcount(), |h| h.count);
        self.finish_screen_motion(if down { Motion::Down } else { Motion::Up }, total, false)
    }

    /// Remember a [`Feed::ScreenMotion`] [`feed`](Self::feed) handed out, so its finisher records the
    /// change for `.` as `feed` records any other command; `mode` is `None` when it completes an
    /// `i_CTRL-O` one-shot. A handoff `.` itself made (it carries the change to repeat) is kept.
    pub(super) fn arm_screen_handoff(
        &mut self,
        motion: ScreenMotion,
        count: u32,
        mode: Option<Mode>,
    ) {
        if self.screen_handoff.is_none() {
            self.screen_handoff = Some(ScreenHandoff {
                motion,
                count,
                mode,
                redo: None,
            });
        }
    }

    /// `.` of a change over a screen-line motion (#1539): hand the motion back to the frontend with the
    /// recorded count, or the `.`'s own, so it resolves again from where the cursor is now. The
    /// finisher then replays `intent` over the new span.
    pub(super) fn redo_screen_motion(
        &mut self,
        intent: ChangeIntent,
        motion: ScreenMotion,
        count: u32,
    ) -> Feed {
        self.screen_handoff = Some(ScreenHandoff {
            motion,
            count,
            mode: None,
            redo: Some(intent),
        });
        Feed::ScreenMotion { motion, count }
    }

    /// The finished screen-line motion as `m` with count `total`. Under `.`, the repeated change over
    /// it. Otherwise the bare move or the armed operator over it, recorded for `.` with the screen
    /// motion, so a later `.` resolves the motion again instead of reusing this span. `inclusive` makes
    /// the operator's exclusive motion inclusive ([`repeat::toggle_inclusive`]); the change is recorded
    /// without it, so a `.` takes it only when the motion it resolves again is inclusive too.
    fn finish_screen_motion(&mut self, m: Motion, total: u32, inclusive: bool) -> Feed {
        let incl = |out: Feed| match out {
            Feed::Cmd(cmd) if inclusive => Feed::Cmd(repeat::toggle_inclusive(&cmd)),
            out => out,
        };
        let handoff = self.screen_handoff.take();
        if let Some(ScreenHandoff {
            redo: Some(mut intent),
            ..
        }) = handoff
        {
            self.reset();
            intent.lead = repeat::with_motion(&intent.lead, m, total);
            if inclusive {
                intent.lead = repeat::toggle_inclusive(&intent.lead);
            }
            return intent.replay(None).into_feed();
        }
        let out = self.op_over_motion(m, total);
        if let Some(h) = handoff {
            match h.mode {
                Some(mode) => self.record(&out, mode),
                // As `feed` records the command that completes an `i_CTRL-O` one-shot.
                None => {
                    self.record(&out, Mode::Normal);
                    if self.recording.is_some() {
                        self.insert.resumed = false;
                    }
                }
            }
            // `record` replaced the change (it keeps the old one to restore): tag the new one, the open
            // recording of a `cgj` or the finished change of a `dgj`.
            if self.replaced_change.is_some() {
                if let Some(rec) = self.recording.as_mut().or(self.last_change.as_mut()) {
                    rec.screen = Some((h.motion, h.count));
                }
            }
        }
        incl(out)
    }

    /// Abandon a [`Feed::ScreenMotion`] that cannot move (`gj` on the last screen row): nvim beeps and
    /// drops the operator.
    pub fn screen_motion_fail(&mut self) -> Feed {
        // Under `.` the change stays the one to repeat, as nvim keeps its redo buffer.
        self.screen_handoff = None;
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
