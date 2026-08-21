//! Frontend command/ex dispatch: run a `Command` (recording it + performing its `Effect`s), execute a
//! parsed `:` line, and the save/label helpers they call. These are thin callers over `Workspace`,
//! `persist::`, and the status/quit sinks — no event-loop or terminal state.

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ruse_core::{Command, DocumentId, Effect, SplitDir, Trace, Workspace};

use crate::highlight;
use crate::input::{BufTarget, Ex};
use crate::persist;
use crate::ui::prompts::Confirm;

/// The per-buffer syntax-highlighter registry (F-007 / F-015), keyed by [`DocumentId`]. Shared by the
/// session loop and the LSP coordinator (both open buffers that need a highlighter).
pub(crate) type Highlighters = HashMap<DocumentId, highlight::CachedHighlight>;

/// Open `file` into a NEW buffer, focus it, register its file identity + highlighter, and return a status
/// line. Shared by the file picker / `:e` (session) and goto/rename/references (LSP coordinator). `E484`
/// on a read error.
pub(crate) fn open_file_into_buffer(
    file: &str,
    ws: &mut Workspace,
    files: &mut Files,
    highlighters: &mut Highlighters,
) -> String {
    let p = PathBuf::from(file);
    match fs::read(&p) {
        Ok(raw) => {
            let f = persist::encoding::FileFormat::detect(&raw);
            let clean = f.to_buffer(&raw);
            let id = ws.add_buffer(clean, Some(file.to_string()));
            ws.focus_buffer(id);
            if let Some(h) = p
                .extension()
                .and_then(|e| e.to_str())
                .and_then(highlight::CachedHighlight::for_ext)
            {
                highlighters.insert(id, h);
            }
            files.insert(id, BufferFile { path: p, fmt: f });
            format!("\"{file}\" {} bytes", raw.len())
        }
        Err(e) => format!("E484: can't open {file}: {e}"),
    }
}

/// Whether `key` is `CTRL-<c>`. Shared frontend key predicate.
pub(crate) fn is_ctrl(key: KeyEvent, c: char) -> bool {
    key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char(c)
}

/// One file buffer's on-disk identity: its path and the detected encoding/line-ending to restore on save
/// (F-008). Held frontend-side (core is IO-free), keyed by [`DocumentId`] in a [`Files`] registry.
pub(crate) struct BufferFile {
    pub(crate) path: PathBuf,
    pub(crate) fmt: persist::encoding::FileFormat,
}

/// The per-buffer file registry: which buffers have a file on disk (a scratch `:enew` buffer has none, so
/// `:w` on it declines). Replaces the old single `path`/`fmt`/`file_buf` triple (F-007 multi-buffer).
pub(crate) type Files = HashMap<DocumentId, BufferFile>;

/// Record a command and apply it to the focused window, performing any effects.
pub(crate) fn run_cmd(
    cmd: Command,
    ws: &mut Workspace,
    files: &Files,
    recorded: &mut Vec<Command>,
    status: &mut String,
    quit: &mut bool,
) {
    recorded.push(cmd.clone());
    for eff in ws.apply(&cmd) {
        apply_effect(eff, ws, files, status, quit);
    }
}

// The ex-line dispatcher legitimately needs the full editor context (buffer, file identity+format,
// the trace baseline+recording, and the status/quit sinks); grouping them into a struct would only
// move the wiring, not reduce it.
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_ex(
    ex: &Ex,
    ws: &mut Workspace,
    files: &Files,
    initial: &[u8],
    recorded: &[Command],
    status: &mut String,
    quit: &mut bool,
    confirm: &mut Option<Confirm>,
) {
    match ex {
        Ex::Save => save(ws, files, status),
        // `:q` refuses when the focused buffer has unsaved changes (Vim E37); `:q!` discards them.
        Ex::Quit => {
            if ws.focused().doc.is_modified() {
                *status = "E37: No write since last change (add ! to override)".into();
            } else {
                *quit = true;
            }
        }
        Ex::QuitForce => *quit = true,
        Ex::SaveQuit => {
            // `:wq`/`:x` on a buffer with no file errors (save declines) and does NOT quit — matching Vim.
            let had_file = files.contains_key(&ws.focused_buffer());
            save(ws, files, status);
            if had_file {
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
        Ex::Only => {
            let closed = ws.only();
            *status = match closed {
                0 => "already one window".into(),
                1 => "1 window closed".into(),
                n => format!("{n} windows closed"),
            };
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
        // `:[range]d` — delete the range's lines (no range = the current line) as one undo group.
        Ex::Delete(range) => {
            let n = ws.delete_lines(*range);
            *status = if n == 1 {
                "1 line deleted".into()
            } else {
                format!("{n} lines deleted")
            };
        }
        // `:[range]y` — yank the range's lines linewise into the unnamed register (like `yy`).
        Ex::Yank(range) => {
            let n = ws.yank_lines(*range);
            *status = if n == 1 {
                "1 line yanked".into()
            } else {
                format!("{n} lines yanked")
            };
        }
        // `:[range]m {addr}` — move the range's lines to after the destination line.
        Ex::Move(range, dest) => {
            *status = match ws.move_lines(*range, *dest) {
                Some(1) => "1 line moved".into(),
                Some(n) => format!("{n} lines moved"),
                None => "E134: Cannot move a range of lines into itself".into(),
            };
        }
        // `:[range]t {addr}` / `:copy` — copy the range's lines to after the destination line.
        Ex::Copy(range, dest) => {
            *status = match ws.copy_lines(*range, *dest) {
                Some(1) => "1 line copied".into(),
                Some(n) => format!("{n} lines copied"),
                None => "E486: invalid copy".into(),
            };
        }
        // `:[range]sort[!] [n][u]` — sort the range's lines (whole file with no range).
        Ex::Sort(range, spec) => {
            let removed = ws.sort_lines(*range, spec.reverse, spec.numeric, spec.unique);
            *status = if removed > 0 {
                format!("sorted, {removed} fewer lines")
            } else {
                "sorted".into()
            };
        }
        // `:set {option}` — set one editor option on the focused view.
        Ex::Set(opt) => {
            ws.set_option(*opt);
            *status = format!("{opt:?}");
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
        // `:registers` is handled in the run loop (it opens the frontend-owned register-viewer picker).
        Ex::Registers => {}
        // `:lmap`/`:lunmap` are handled in the run loop (they mutate engine-owned Lang-Arg state — the
        // `engine` this fn does not borrow); never reach here.
        Ex::Lmap { .. } | Ex::Lunmap { .. } => {}
        // `:checkhealth` is handled in the run loop (it reads the terminal-cap ledger + profile this fn
        // does not borrow); never reaches here.
        Ex::CheckHealth => {}
        // `:e {file}` / `:e!` are handled in the run loop (they read the file + mutate the buffer and the
        // `files`/highlighter registries this fn only borrows immutably); never reach here.
        Ex::Edit(_) | Ex::EditReload => {}
        // `:bd` is handled in the run loop (it drops the deleted buffer's `files`/highlighter entries,
        // which this fn only borrows immutably); never reaches here.
        Ex::BufferDelete { .. } => {}
        // `:earlier`/`:later` are handled in the run loop (they record + apply undo-time commands into
        // the `recorded` stream this fn only borrows immutably); never reach here.
        Ex::Earlier(_) | Ex::Later(_) => {}
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
        // `:terminal` (F-011) / `:fmt` / `:rename` / `:references` (F-014) are handled in `session::run`
        // (they need the terminals / lsp maps); they never reach here, but the match stays exhaustive.
        Ex::Terminal
        | Ex::Format
        | Ex::Rename(_)
        | Ex::References
        | Ex::CodeAction
        | Ex::Diagnostics => {}
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
    files: &Files,
    status: &mut String,
    quit: &mut bool,
) {
    match eff {
        Effect::Save => save(ws, files, status),
        Effect::Quit => *quit = true,
        Effect::Status(s) => *status = s,
    }
}

/// Buffer line count for the `:w` write report, matching Vim: an empty buffer is 1 line; otherwise it is
/// the newline count, plus one when the last line has no trailing newline (`"a\nb"` and `"a\nb\n"` are both
/// 2 lines).
fn line_count(buf: &[u8]) -> usize {
    if buf.is_empty() {
        return 1;
    }
    buf.iter().filter(|&&b| b == b'\n').count() + usize::from(buf.last() != Some(&b'\n'))
}

pub(crate) fn save(ws: &mut Workspace, files: &Files, status: &mut String) {
    // Multi-buffer honesty (F-007): only a buffer WITH a file writes. A scratch buffer (`:enew`) has no
    // registry entry, so `:w` declines rather than clobbering another buffer's file.
    let Some(bf) = files.get(&ws.focused_buffer()) else {
        *status = "E32: No file name".into();
        return;
    };
    // Restore the original encoding/line-endings (F-008 #2), then write durably (fsync + rename, #1).
    let bytes = bf.fmt.to_disk(ws.focused().doc.bytes());
    match persist::atomic::save(&bf.path, &bytes) {
        Ok(()) => {
            ws.focused_doc_mut().mark_saved();
            persist::journal::clear(Some(bf.path.as_path())); // saved bytes are durable — nothing to recover
            tracing::info!(event = "save", path = %bf.path.display(), bytes = bytes.len());
            // Vim-style write report: `"file" 42L, 1024B written` (L = buffer lines, B = bytes on disk).
            *status = format!(
                "\"{}\" {}L, {}B written",
                bf.path.display(),
                line_count(ws.focused().doc.bytes()),
                bytes.len()
            );
        }
        Err(e) => {
            // Expected external failure (§7): surface it in the status bar, log once, keep the buffer.
            tracing::warn!(event = "save.failed", path = %bf.path.display(), error = %e);
            *status = format!("write failed: {e}");
        }
    }
}

#[cfg(test)]
mod dispatch_tests {
    use super::*;
    use ruse_core::Command;

    /// `:q` refuses (E37) once the focused buffer is modified; `:q!` quits regardless; a clean `:q` quits.
    #[test]
    fn quit_guards_unsaved_changes_and_bang_overrides() {
        let files = Files::new();
        let run = |ex: &Ex, ws: &mut Workspace| {
            let mut status = String::new();
            let mut quit = false;
            let mut confirm = None;
            run_ex(
                ex,
                ws,
                &files,
                b"",
                &[],
                &mut status,
                &mut quit,
                &mut confirm,
            );
            (status, quit)
        };

        // Clean buffer → `:q` quits.
        let mut ws = Workspace::new(b"hello\n".to_vec());
        let (_, quit) = run(&Ex::Quit, &mut ws);
        assert!(quit, "clean buffer quits on :q");

        // Modify the buffer → `:q` refuses with E37, `:q!` quits.
        let mut ws = Workspace::new(b"hello\n".to_vec());
        ws.apply(&Command::EnterInsert);
        ws.apply(&Command::InsertChar('x'));
        assert!(ws.focused().doc.is_modified(), "edit dirtied the buffer");
        let (status, quit) = run(&Ex::Quit, &mut ws);
        assert!(!quit, ":q does not quit a dirty buffer");
        assert!(status.starts_with("E37"), "E37 surfaced: {status}");
        let (_, quit) = run(&Ex::QuitForce, &mut ws);
        assert!(quit, ":q! discards changes and quits");
    }

    /// The `:w` line count matches Vim across the edge cases: empty buffer = 1 line, a trailing newline
    /// does NOT add a phantom empty line, and a missing final newline still counts its line.
    #[test]
    fn line_count_matches_vim() {
        assert_eq!(line_count(b""), 1, "empty buffer is one line");
        assert_eq!(line_count(b"a"), 1, "single line, no newline");
        assert_eq!(line_count(b"a\n"), 1, "trailing newline is not a new line");
        assert_eq!(line_count(b"a\nb"), 2, "missing final newline still counts");
        assert_eq!(line_count(b"a\nb\n"), 2, "two lines with trailing newline");
        assert_eq!(line_count(b"\n"), 1, "a lone newline is one line");
        assert_eq!(line_count(b"\n\n"), 2, "two blank lines");
    }
}
