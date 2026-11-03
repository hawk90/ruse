//! nvim's window-local display options (#615, #616), kept on each [`crate::View`] so a split copies them,
//! as nvim copies a window's options into the window `:split` makes.
//!
//! The core never reads these values. They are plain data the frontend lays out, paints and scrolls with
//! (like the View's `top` and its own `'scrolloff'`); the core only stores them, gives new Views the
//! global copy ([`crate::Workspace::window_defaults`]) and copies them on a split.

/// The window-local display options. Field defaults are nvim v0.12.6's.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WindowOptions {
    /// `'wrap'`: long lines continue on the next screen row.
    pub wrap: bool,
    /// `'linebreak'`: wrap at a `'breakat'` character rather than at the last cell.
    pub linebreak: bool,
    /// `'breakindent'`: wrapped rows keep the line's indent.
    pub breakindent: bool,
    /// `'breakindentopt'`, as typed (comma-separated `min:`, `shift:`, `sbr`, `list:`, `column:`).
    pub breakindentopt: String,
    /// `'number'`.
    pub number: bool,
    /// `'relativenumber'`.
    pub relativenumber: bool,
    /// `'numberwidth'`: the number column's minimal width.
    pub numberwidth: i64,
    /// `'signcolumn'`, as typed (`auto`, `no`, `yes`, `number`, `auto:N`, `yes:N`, `auto:N-M`).
    pub signcolumn: String,
    /// `'list'`: show the `'listchars'` characters.
    pub list: bool,
    /// The window's own `'sidescrolloff'` (global-local; `None` follows the global value).
    pub sidescrolloff: Option<i64>,
    /// The window's own `'showbreak'` (global-local; `None` follows the global value).
    pub showbreak: Option<String>,
    /// The window's own `'listchars'` (global-local; `None` follows the global value).
    pub listchars: Option<String>,
    /// `'cursorline'`: highlight the cursor's line.
    pub cursorline: bool,
    /// `'cursorlineopt'`, as typed (`line`, `screenline`, `number`, `both`).
    pub cursorlineopt: String,
    /// `'cursorcolumn'`: highlight the cursor's screen column.
    pub cursorcolumn: bool,
    /// `'colorcolumn'`, as typed (columns, `+N` / `-N` from `'textwidth'`).
    pub colorcolumn: String,
    /// `'foldcolumn'`, as typed (`0`-`9`, `auto`, `auto:N`).
    pub foldcolumn: String,
    /// `'smoothscroll'`: scroll a wrapped line by screen rows.
    pub smoothscroll: bool,
    /// The window's own `'fillchars'` (global-local; `None` follows the global value).
    pub fillchars: Option<String>,
}

impl Default for WindowOptions {
    fn default() -> WindowOptions {
        WindowOptions {
            wrap: true,
            linebreak: false,
            breakindent: false,
            breakindentopt: String::new(),
            number: false,
            relativenumber: false,
            numberwidth: 4,
            signcolumn: "auto".into(),
            list: false,
            sidescrolloff: None,
            showbreak: None,
            listchars: None,
            cursorline: false,
            cursorlineopt: "both".into(),
            cursorcolumn: false,
            colorcolumn: String::new(),
            foldcolumn: "0".into(),
            smoothscroll: false,
            fillchars: None,
        }
    }
}
