//! Leaving a buffer with unsaved changes (#619), as nvim v0.12.6's `check_changed`, `can_abandon` and
//! `dialog_changed` decide it: `'hidden'` (a buffer no window shows stays loaded with its changes),
//! `'autowrite'` / `'autowriteall'` (write it instead), and `'confirm'` / `:confirm` (ask instead of
//! failing). The gate runs before the Ex commands that leave a buffer: `:e {other}`, `:enew`, `:b`,
//! `:bn`, `:bp`, `:q` / `:close` of a window, `:only`, `:qa`, and the shell commands `'autowrite'` writes
//! for (`:!`, `:r !`, `:{range}!`).
//!
//! The dialog is nvim's console dialog on one status line: `Save changes to "f.txt"?` and
//! `[Y]es, (N)o, (C)ancel: `, or with several changed buffers `[Y]es, (N)o, Save (A)ll, (D)iscard All,
//! (C)ancel: `. `y` (and `<CR>`, the default) writes the buffer, `n` marks it unchanged (nvim's
//! `unchanged()`: the text stays, the buffer no longer counts as modified), `a` writes all, `d` marks all
//! unchanged; then the command runs again, so a `:qa` asks for each buffer in turn. `c`, `<Esc>` and
//! `CTRL-C` cancel; any other key is ignored while the prompt stays up.

use ruse_core::{DocumentId, Workspace};

use crate::app::dispatch::Files;
use crate::app::messages::Status;
use crate::input::{BufTarget, Ex, ReadSource};

/// The buffer-lifecycle options (`'hidden'`, `'confirm'`, `'autowrite'`, `'autowriteall'`,
/// `'autoread'`), at nvim's defaults.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct BufferOptions {
    pub hidden: bool,
    pub confirm: bool,
    pub autowrite: bool,
    pub autowriteall: bool,
    pub autoread: bool,
}

impl Default for BufferOptions {
    fn default() -> BufferOptions {
        BufferOptions {
            hidden: true,
            confirm: false,
            autowrite: false,
            autowriteall: false,
            autoread: true,
        }
    }
}

impl BufferOptions {
    /// `'autowrite'`, which `'autowriteall'` implies.
    fn aw(&self) -> bool {
        self.autowrite || self.autowriteall
    }
}

/// A `:confirm` / `'confirm'` dialog waiting for its key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ChangedPrompt {
    /// The buffer the question names.
    pub buf: DocumentId,
    /// Several buffers are changed: `Save All` / `Discard All` are offered.
    pub checkall: bool,
    /// The command line to run again once answered, then the rest of the `|` line.
    pub pending: Vec<String>,
}

/// What the gate decided.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Gate {
    /// The command runs.
    Go,
    /// The command does not run; the status says why.
    Stop,
    /// The command waits for the dialog's answer.
    Ask(ChangedPrompt),
}

/// The prompt text of `p`.
pub(crate) fn prompt_text(p: &ChangedPrompt, ws: &Workspace) -> String {
    let name = ws.buffer_name(p.buf).unwrap_or("Untitled");
    let buttons = if p.checkall {
        "[Y]es, (N)o, Save (A)ll, (D)iscard All, (C)ancel: "
    } else {
        "[Y]es, (N)o, (C)ancel: "
    };
    format!("Save changes to \"{name}\"? {buttons}")
}

fn modified(ws: &Workspace, id: DocumentId) -> bool {
    ws.document(id).is_some_and(|d| d.is_modified())
}

/// How many windows show `id`.
fn windows_showing(ws: &Workspace, id: DocumentId) -> usize {
    ws.window_buffers().iter().filter(|&&b| b == id).count()
}

/// The changed buffers in nvim's order (`check_changed_any`): the focused one, those in other windows,
/// then the rest.
fn changed_buffers(ws: &Workspace) -> Vec<DocumentId> {
    let mut order = vec![ws.focused_buffer()];
    for b in ws.window_buffers() {
        if !order.contains(&b) {
            order.push(b);
        }
    }
    for b in ws.buffers() {
        if !order.contains(&b.id) {
            order.push(b.id);
        }
    }
    order.into_iter().filter(|&b| modified(ws, b)).collect()
}

/// Write the buffer `id` (nvim's `buf_write_all`), as `:wa` would write it alone: a buffer without a file
/// or a `'readonly'` one is not written. Returns whether it is unchanged afterwards.
pub(crate) fn write_buffer(
    ws: &mut Workspace,
    files: &Files,
    id: DocumentId,
    status: &mut Status,
) -> bool {
    crate::app::file_cmds::ex_write_all(ws, files, status, |b| b != id);
    !modified(ws, id)
}

/// `'autowrite'`: write the changed buffers among `ids` that have a file and are not `'readonly'`
/// (nvim's `autowrite`, which never writes those).
fn autowrite(ws: &mut Workspace, files: &Files, ids: &[DocumentId], status: &mut Status) {
    if !files.write.write {
        return;
    }
    let writable: Vec<DocumentId> = ids
        .iter()
        .copied()
        .filter(|id| files.get(id).is_some_and(|bf| !bf.read_only))
        .collect();
    if writable.is_empty() {
        return;
    }
    crate::app::file_cmds::ex_write_all(ws, files, status, |b| !writable.contains(&b));
}

/// What a command does to the buffers it leaves.
enum Leaves {
    /// The focused window shows another buffer (`:e`, `:enew`, `:b`): with `aw`, `'autowrite'` applies.
    Focused {
        aw: bool,
    },
    /// The focused window closes, other windows stay (`:q`, `:close`).
    Window {
        aw: bool,
    },
    /// The editor ends (`:q` on the last window, `:qa`).
    All {
        aw: bool,
    },
    /// The other windows close (`:only`).
    Others,
    /// A shell command runs: `'autowrite'` writes every changed buffer first.
    Shell,
    Nothing,
}

fn leaves(ex: &Ex, ws: &Workspace, files: &Files, opts: &BufferOptions) -> Leaves {
    let focused = ws.focused_buffer();
    match ex {
        Ex::Edit(spec) if !spec.bang => match spec.file.as_deref() {
            Some(f)
                if crate::app::dispatch::buffer_for_path(files, std::path::Path::new(f))
                    != Some(focused) =>
            {
                Leaves::Focused {
                    aw: opts.autowriteall,
                }
            }
            _ => Leaves::Nothing,
        },
        Ex::Enew => Leaves::Focused {
            aw: opts.autowriteall,
        },
        Ex::Buffer(BufTarget::Number(n)) if *n == focused.0 => Leaves::Nothing,
        Ex::Buffer(_) | Ex::BufferNext | Ex::BufferPrev if ws.buffers().len() > 1 => {
            Leaves::Focused { aw: opts.aw() }
        }
        Ex::Quit if ws.window_count() > 1 => Leaves::Window {
            aw: opts.autowriteall,
        },
        Ex::Close if ws.window_count() > 1 => Leaves::Window { aw: false },
        Ex::Quit | Ex::QuitAll { force: false } => Leaves::All {
            aw: opts.autowriteall,
        },
        Ex::Only => Leaves::Others,
        Ex::Shell(_)
        | Ex::Filter { .. }
        | Ex::Read {
            source: ReadSource::Command(_),
            ..
        } => Leaves::Shell,
        _ => Leaves::Nothing,
    }
}

/// The unsaved-changes gate before `ex` runs. `confirm` is `:confirm` on this command; `line` is the
/// command line to run again after a dialog, `rest` the `|` commands after it.
#[allow(clippy::too_many_arguments)]
pub(crate) fn gate(
    ex: &Ex,
    ws: &mut Workspace,
    files: &Files,
    opts: &BufferOptions,
    confirm: bool,
    line: &str,
    status: &mut Status,
) -> Gate {
    let ask = (confirm || opts.confirm) && files.write.write;
    let dialog = |buf: DocumentId, checkall: bool| {
        Gate::Ask(ChangedPrompt {
            buf,
            checkall,
            pending: vec![line.to_string()],
        })
    };
    let focused = ws.focused_buffer();
    match leaves(ex, ws, files, opts) {
        Leaves::Nothing => Gate::Go,
        Leaves::Shell => {
            if opts.aw() {
                let all = changed_buffers(ws);
                autowrite(ws, files, &all, status);
            }
            Gate::Go
        }
        Leaves::Focused { aw } | Leaves::Window { aw } => {
            if opts.hidden || !modified(ws, focused) || windows_showing(ws, focused) > 1 {
                return Gate::Go;
            }
            if aw {
                autowrite(ws, files, &[focused], status);
                if !modified(ws, focused) {
                    return Gate::Go;
                }
            }
            if ask {
                return dialog(focused, false);
            }
            status.error("E37: No write since last change (add ! to override)");
            Gate::Stop
        }
        Leaves::All { aw } => {
            if aw {
                let all = changed_buffers(ws);
                autowrite(ws, files, &all, status);
            }
            let left = changed_buffers(ws);
            match left.first() {
                Some(&first) if ask => dialog(first, left.len() > 1),
                // The quit itself says E37 / E162.
                _ => Gate::Go,
            }
        }
        Leaves::Others => {
            if opts.hidden {
                return Gate::Go;
            }
            let shown = ws.window_buffers();
            let doomed: Vec<DocumentId> = shown
                .iter()
                .enumerate()
                .filter(|&(i, &b)| i != ws.focus() && b != focused && modified(ws, b))
                .map(|(_, &b)| b)
                .collect();
            if doomed.is_empty() {
                return Gate::Go;
            }
            if opts.aw() {
                autowrite(ws, files, &doomed, status);
            }
            match doomed.iter().copied().find(|&b| modified(ws, b)) {
                None => Gate::Go,
                Some(b) if ask => dialog(b, false),
                Some(_) => {
                    status.error("E445: Other window contains changes");
                    Gate::Stop
                }
            }
        }
    }
}

/// The answer a key gives a dialog.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Answer {
    Yes,
    No,
    All,
    DiscardAll,
    Cancel,
}

/// nvim's `do_dialog` keys: the hotkeys (case-insensitive), `<CR>` for the default (Yes), `<Esc>` and
/// `CTRL-C` to cancel; anything else is no answer.
pub(crate) fn answer(p: &ChangedPrompt, key: crossterm::event::KeyEvent) -> Option<Answer> {
    use crossterm::event::{KeyCode, KeyModifiers};
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    match key.code {
        KeyCode::Char('c') if ctrl => Some(Answer::Cancel),
        KeyCode::Esc => Some(Answer::Cancel),
        KeyCode::Enter => Some(Answer::Yes),
        KeyCode::Char(c) if !ctrl => match c.to_ascii_lowercase() {
            'y' => Some(Answer::Yes),
            'n' => Some(Answer::No),
            'c' => Some(Answer::Cancel),
            'a' if p.checkall => Some(Answer::All),
            'd' if p.checkall => Some(Answer::DiscardAll),
            _ => None,
        },
        _ => None,
    }
}

/// Carry out `a` for `p`; returns whether the command runs again.
pub(crate) fn act(
    p: &ChangedPrompt,
    a: Answer,
    ws: &mut Workspace,
    files: &Files,
    status: &mut Status,
) -> bool {
    match a {
        Answer::Yes => {
            if files.get(&p.buf).is_none() {
                status.error(format!("E141: No file name for buffer {}", p.buf.0));
                return false;
            }
            write_buffer(ws, files, p.buf, status)
        }
        Answer::No => {
            if let Some(d) = ws.document_mut(p.buf) {
                d.mark_saved();
            }
            true
        }
        Answer::All => {
            crate::app::file_cmds::ex_write_all(ws, files, status, |b| {
                files.get(&b).is_some_and(|bf| bf.read_only)
            });
            true
        }
        Answer::DiscardAll => {
            for b in changed_buffers(ws) {
                if let Some(d) = ws.document_mut(b) {
                    d.mark_saved();
                }
            }
            true
        }
        Answer::Cancel => false,
    }
}
