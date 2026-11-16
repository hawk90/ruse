//! The display family: the session's display globals, the global-local display options
//! (`'showbreak'`, `'listchars'`, `'fillchars'`, `'sidescrolloff'`) and the scroll options (`'scroll'`,
//! `'scrolloff'`, `'sidescroll'`, `'scrolljump'`).

use ruse_core::{OptionLevel, WindowOptions};

use super::OptionHost;
use crate::input::set_cmd::{Opt, OptVal};

impl OptionHost<'_> {
    /// The value of a display-family option; `local` is the focused window's options unless `global`.
    pub(super) fn get_display(
        &self,
        opt: Opt,
        global: bool,
        local: Option<&WindowOptions>,
    ) -> OptVal {
        let d = &self.ui.display;
        match opt {
            // Global-local: the window's own value when it has one, else the global one.
            Opt::ShowBreak => OptVal::Text(
                local
                    .and_then(|o| o.showbreak.clone())
                    .unwrap_or_else(|| d.showbreak.clone())
                    .into(),
            ),
            Opt::ListChars => OptVal::Text(
                local
                    .and_then(|o| o.listchars.clone())
                    .unwrap_or_else(|| d.listchars.clone())
                    .into(),
            ),
            Opt::FillChars => OptVal::Text(
                local
                    .and_then(|o| o.fillchars.clone())
                    .unwrap_or_else(|| d.fillchars.clone())
                    .into(),
            ),
            // 'scroll': the window's value at its height (half the height until a count or `:set` gives
            // it one).
            Opt::Scroll => OptVal::Num(
                i64::try_from(self.ws.focused().view.scroll(self.win_height)).unwrap_or(0),
            ),
            Opt::SideScrollOff => OptVal::Num(
                local
                    .and_then(|o| o.sidescrolloff)
                    .unwrap_or(d.sidescrolloff),
            ),
            Opt::BreakAt => OptVal::Text(d.breakat.clone().into()),
            Opt::Display => OptVal::Text(d.display.clone().into()),
            Opt::SideScroll => OptVal::Num(d.sidescroll),
            Opt::ScrollJump => OptVal::Num(d.scrolljump),
            // The window's own value, else the global one (`:set so?` shows the value in force).
            Opt::ScrollOff if global => OptVal::Num(*self.scrolloff),
            Opt::ScrollOff => OptVal::Num(
                self.ws
                    .focused()
                    .view
                    .scrolloff_local()
                    .unwrap_or(*self.scrolloff),
            ),
            _ => unreachable!(),
        }
    }

    /// The session's display globals, the global-local display options and the scroll options.
    pub(super) fn put_display(&mut self, opt: Opt, level: OptionLevel, val: OptVal, text: String) {
        match (opt, val) {
            // The global-local display options: as 'scrolloff' (`:set` drops the window's own value).
            (Opt::ShowBreak | Opt::ListChars | Opt::FillChars | Opt::SideScrollOff, v) => {
                let n = match v {
                    OptVal::Num(n) => n,
                    _ => 0,
                };
                if level != OptionLevel::Global {
                    let local = level == OptionLevel::Local;
                    self.ws
                        .update_window_options(OptionLevel::Local, |o| match opt {
                            Opt::ShowBreak => o.showbreak = local.then(|| text.clone()),
                            Opt::ListChars => o.listchars = local.then(|| text.clone()),
                            Opt::FillChars => o.fillchars = local.then(|| text.clone()),
                            _ => o.sidescrolloff = local.then_some(n),
                        });
                }
                if level != OptionLevel::Local {
                    let d = &mut self.ui.display;
                    match opt {
                        Opt::ShowBreak => d.showbreak = text,
                        Opt::ListChars => d.listchars = text,
                        Opt::FillChars => d.fillchars = text,
                        _ => d.sidescrolloff = n,
                    }
                }
            }
            (Opt::BreakAt, _) => self.ui.display.breakat = text,
            (Opt::Display, _) => self.ui.display.display = text,
            (Opt::SideScroll, OptVal::Num(n)) => self.ui.display.sidescroll = n,
            (Opt::ScrollJump, OptVal::Num(n)) => self.ui.display.scrolljump = n,
            // 'scroll' 0 is half the window again (nvim's `win_comp_scroll`).
            (Opt::Scroll, OptVal::Num(n)) => {
                let h = self.win_height.max(1);
                let lines = usize::try_from(n).unwrap_or(0);
                let lines = if lines == 0 { (h / 2).max(1) } else { lines };
                let focus = self.ws.focus();
                self.ws.set_scroll(focus, lines, h);
            }
            // Global-local: `:setlocal` gives the focused window its own value, `:set` sets the global
            // value and drops the window's (nvim), `:setglobal` leaves the window's alone.
            (Opt::ScrollOff, OptVal::Num(n)) => match level {
                OptionLevel::Local => self.ws.set_focused_scrolloff(Some(n)),
                OptionLevel::Both => {
                    *self.scrolloff = n;
                    self.ws.set_focused_scrolloff(None);
                }
                OptionLevel::Global => *self.scrolloff = n,
            },
            _ => {}
        }
    }
}
