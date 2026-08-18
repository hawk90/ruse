//! Frontend command/ex dispatch: run a `Command` (recording it + performing its `Effect`s), execute a
//! parsed `:` line, and the save/label helpers they call. These are thin callers over `Workspace`,
//! `persist::`, and the status/quit sinks — no event-loop or terminal state.

use std::fs;
use std::path::PathBuf;

use ruse_core::{Command, Effect, SplitDir, Trace, Workspace};

use crate::input::{BufTarget, Ex};
use crate::persist;
use crate::ui::prompts::Confirm;

/// Record a command and apply it to the focused window, performing any effects.
#[allow(clippy::too_many_arguments)] // the command path needs the full editor context (buffer/file/sinks)
pub(crate) fn run_cmd(
    cmd: Command,
    ws: &mut Workspace,
    path: &Option<PathBuf>,
    fmt: persist::encoding::FileFormat,
    file_buf: ruse_core::DocumentId,
    recorded: &mut Vec<Command>,
    status: &mut String,
    quit: &mut bool,
) {
    recorded.push(cmd.clone());
    for eff in ws.apply(&cmd) {
        apply_effect(eff, ws, path, fmt, file_buf, status, quit);
    }
}

// The ex-line dispatcher legitimately needs the full editor context (buffer, file identity+format,
// the trace baseline+recording, and the status/quit sinks); grouping them into a struct would only
// move the wiring, not reduce it.
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_ex(
    ex: &Ex,
    ws: &mut Workspace,
    path: &Option<PathBuf>,
    fmt: persist::encoding::FileFormat,
    file_buf: ruse_core::DocumentId,
    initial: &[u8],
    recorded: &[Command],
    status: &mut String,
    quit: &mut bool,
    confirm: &mut Option<Confirm>,
) {
    match ex {
        Ex::Save => save(ws, path, fmt, file_buf, status),
        Ex::Quit => *quit = true,
        Ex::SaveQuit => {
            // `:wq`/`:x` on a scratch buffer errors (save declines) and does NOT quit — matching Vim.
            save(ws, path, fmt, file_buf, status);
            if ws.focused_buffer() == file_buf {
                *quit = true;
            }
        }
        // F-007 window commands: split the focused window onto the same buffer, or close it.
        Ex::Split => {
            ws.split(SplitDir::Horizontal);
            *status = "window split".into();
        }
        Ex::VSplit => {
            ws.split(SplitDir::Vertical);
            *status = "window vsplit".into();
        }
        Ex::Close => {
            if !ws.close_focused() {
                *status = "cannot close last window (:q to quit)".into();
            }
        }
        Ex::SaveTrace(p) => {
            let trace = Trace::record(initial, recorded.to_vec());
            match fs::write(p, trace.to_text()) {
                Ok(()) => *status = format!("trace saved: {p} ({} commands)", recorded.len()),
                Err(e) => *status = format!("trace save failed: {e}"),
            }
        }
        // `:[range]s/pat/rep/flags` (F-009 #2). The `c` (confirm) flag is the interactive loop (PR-c2);
        // for now it is declined rather than silently applied.
        Ex::Substitute(spec) => {
            let flags = ruse_core::SubFlags {
                global: spec.global,
                ignore_case: spec.ignore_case,
            };
            if spec.confirm {
                // `c`: compute the matches and hand control to the interactive confirm loop (F-009 #2).
                match ws.substitute_preview(spec.range, &spec.pattern, &spec.replacement, flags) {
                    Ok(subs) if subs.is_empty() => {
                        *status = format!("E486: pattern not found: {}", spec.pattern);
                    }
                    Ok(subs) => *confirm = Some(Confirm::new(subs)),
                    Err(e) => *status = regex_error_msg(&e),
                }
            } else {
                *status = match ws.substitute(spec.range, &spec.pattern, &spec.replacement, flags) {
                    Ok(out) if out.replacements == 0 => {
                        format!("E486: pattern not found: {}", spec.pattern)
                    }
                    Ok(out) => format!("{} substitutions on {} lines", out.replacements, out.lines),
                    Err(e) => regex_error_msg(&e),
                };
            }
        }
        // `:[range]g/pat/cmd` (F-009 #4): two-pass mark-then-execute over the focused window.
        Ex::Global(spec) => {
            *status = match ws.global(spec.range, &spec.pattern, spec.negate, &spec.cmd) {
                Ok(0) => format!("E486: pattern not found: {}", spec.pattern),
                Ok(n) => format!("{n} lines changed"),
                Err(e) => regex_error_msg(&e),
            };
        }
        // `:noh` is handled in the run loop (it clears the frontend's search highlight); never reaches here.
        Ex::NoHighlight => {}
        // `:lmap`/`:lunmap` are handled in the run loop (they mutate engine-owned Lang-Arg state — the
        // `engine` this fn does not borrow); never reach here.
        Ex::Lmap { .. } | Ex::Lunmap { .. } => {}
        // `:checkhealth` is handled in the run loop (it reads the terminal-cap ledger + profile this fn
        // does not borrow); never reaches here.
        Ex::CheckHealth => {}
        // F-007 multi-buffer navigation. `:enew` opens a scratch buffer and focuses it; `:ls` lists the
        // buffers; `:bn`/`:bp` cycle; `:b {n}`/`:b#` switch by number / to the alternate.
        Ex::Enew => {
            let id = ws.add_buffer(Vec::new(), None);
            ws.focus_buffer(id);
            *status = format!("[No Name] (buffer {})", id.0);
        }
        Ex::Buffers => *status = buffer_list_line(ws),
        Ex::BufferNext => {
            ws.cycle_buffer(true);
            *status = focused_buffer_label(ws);
        }
        Ex::BufferPrev => {
            ws.cycle_buffer(false);
            *status = focused_buffer_label(ws);
        }
        Ex::Buffer(target) => {
            let id = match target {
                BufTarget::Number(n) => Some(ruse_core::DocumentId(*n)),
                BufTarget::Alternate => ws.alternate(),
            };
            match id {
                Some(id) if ws.focus_buffer(id) => *status = focused_buffer_label(ws),
                Some(id) => *status = format!("E86: buffer {} does not exist", id.0),
                None => *status = "E23: no alternate buffer".into(),
            }
        }
        Ex::Unknown(s) => *status = format!("unknown command: {s}"),
    }
}

/// A short label for the focused buffer (name + `[+]` when modified) — the status after `:b`/`:bn`/`:bp`.
pub(crate) fn focused_buffer_label(ws: &Workspace) -> String {
    let id = ws.focused_buffer();
    let name = ws.buffer_name(id).unwrap_or("[No Name]");
    let dirty = if ws.focused().doc.is_modified() {
        " [+]"
    } else {
        ""
    };
    format!("buffer {}: {name}{dirty}", id.0)
}

/// The one-line `:ls` buffer list: `1%a name  2  # other  3 + scratch …` (`%`=current, `#`=alternate,
/// `+`=modified). Vim renders a multi-line list; the single status line shows a compact form for MVP.
pub(crate) fn buffer_list_line(ws: &Workspace) -> String {
    ws.buffers()
        .iter()
        .map(|b| {
            let cur = if b.current { "%" } else { "" };
            let alt = if b.alt { "#" } else { "" };
            let modified = if b.modified { "+" } else { "" };
            format!("{}{cur}{alt} {}{modified}", b.id.0, b.name)
        })
        .collect::<Vec<_>>()
        .join("  ")
}

/// Human-readable status for a regex compile error (F-009).
pub(crate) fn regex_error_msg(e: &ruse_core::RegexError) -> String {
    match e {
        ruse_core::RegexError::Unsupported(m) => format!("E: unsupported pattern: {m}"),
        ruse_core::RegexError::Syntax(m) => format!("E: bad pattern: {m}"),
    }
}

pub(crate) fn apply_effect(
    eff: Effect,
    ws: &mut Workspace,
    path: &Option<PathBuf>,
    fmt: persist::encoding::FileFormat,
    file_buf: ruse_core::DocumentId,
    status: &mut String,
    quit: &mut bool,
) {
    match eff {
        Effect::Save => save(ws, path, fmt, file_buf, status),
        Effect::Quit => *quit = true,
        Effect::Status(s) => *status = s,
    }
}

pub(crate) fn save(
    ws: &mut Workspace,
    path: &Option<PathBuf>,
    fmt: persist::encoding::FileFormat,
    file_buf: ruse_core::DocumentId,
    status: &mut String,
) {
    // Multi-buffer honesty (F-007): `path`/`fmt` belong to the ONE file buffer. A scratch buffer (`:enew`)
    // or any other buffer has no file, so `:w` must not write its bytes over the original file.
    if ws.focused_buffer() != file_buf {
        *status = "E32: No file name (scratch buffer)".into();
        return;
    }
    let Some(p) = path else {
        *status = "no file name (open with `ruse <file>`)".into();
        return;
    };
    // Restore the original encoding/line-endings (F-008 #2), then write durably (fsync + rename, #1).
    let bytes = fmt.to_disk(ws.focused().doc.bytes());
    match persist::atomic::save(p, &bytes) {
        Ok(()) => {
            ws.focused_doc_mut().mark_saved();
            persist::journal::clear(path.as_deref()); // saved bytes are durable — no work to recover
            tracing::info!(event = "save", path = %p.display(), bytes = bytes.len());
            *status = format!("\"{}\" written", p.display());
        }
        Err(e) => {
            // Expected external failure (§7): surface it in the status bar, log once, keep the buffer.
            tracing::warn!(event = "save.failed", path = %p.display(), error = %e);
            *status = format!("write failed: {e}");
        }
    }
}
