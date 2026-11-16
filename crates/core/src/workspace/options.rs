//! Option levels: the global copy every new View takes and the focused View's local copy.

use crate::Workspace;

impl Workspace {
    /// Set the workspace default search-case and apply it to EVERY live view (existing and, via
    /// [`Self::push_view`], any created afterward). The frontend calls this once at startup
    /// to install ruse's shipped default (ignorecase + smartcase ON) so it holds across `:e`/`:split`/reload
    /// — without touching `EditorState::new`, which the parity oracle drives at the Vim factory default.
    pub fn set_default_search_case(&mut self, ignore_case: bool, smart_case: bool) {
        self.defaults.set_search_case(ignore_case, smart_case);
        for v in self.views.iter_mut().flatten() {
            v.update_options(|o| o.set_search_case(ignore_case, smart_case));
        }
    }

    /// Set the indent config (`>>`/`<<`) of the focused View and of every View created afterward (the
    /// global value, as `:set` writes) — the seam a config loader/test uses; mirrors
    /// [`EditorState::set_indent`]. No new schema key (editor.tab_width / editor.indent_style).
    pub fn set_indent(&mut self, tab_width: usize, indent_style: crate::editor::IndentStyle) {
        self.defaults.set_indent(tab_width, indent_style);
        if let Some(v) = self.window_view_mut(self.focus) {
            v.update_options(|o| o.set_indent(tab_width, indent_style));
        }
    }

    /// Set one `:set` option on the focused view and as the global value every View created afterward
    /// takes (Vim's `:set` writes both the local and the global value, #754).
    pub fn set_option(&mut self, opt: crate::editor::EditorOption) {
        self.set_option_in(opt, OptionLevel::Both);
    }

    /// Set one option at `level` (`:set`, `:setlocal` or `:setglobal`). A local value lives on the
    /// focused View, the global one on [`Workspace::defaults`], which every View created afterward takes.
    /// A Vim-global option ([`crate::editor::EditorOption::is_global`]) has one value, so every level
    /// writes the global value AND every open View.
    pub fn set_option_in(&mut self, opt: crate::editor::EditorOption, level: OptionLevel) {
        if opt.is_global() {
            self.defaults.set(opt.clone());
            for v in self.views.iter_mut().flatten() {
                v.update_options(|o| o.set(opt.clone()));
            }
            return;
        }
        if level != OptionLevel::Local {
            self.defaults.set(opt.clone());
        }
        if level != OptionLevel::Global {
            if let Some(v) = self.window_view_mut(self.focus) {
                v.update_options(|o| o.set(opt));
            }
        }
    }

    /// The GLOBAL values of the options the core reads (the `:setglobal` copy new Views take), in the
    /// order of [`EditorState::editing_options`]. The focused View's local values are
    /// `self.focused().view.editing_options()`.
    #[must_use]
    pub fn global_editing_options(&self) -> Vec<crate::editor::EditorOption> {
        self.defaults.editing_options()
    }
}

/// Which copy of an option a set writes: Vim's `:set` (both), `:setlocal` or `:setglobal`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum OptionLevel {
    /// `:set` — the focused View's local value and the global value.
    Both,
    /// `:setlocal` — the focused View's local value only.
    Local,
    /// `:setglobal` — the global value only (what Views created afterward take).
    Global,
}
