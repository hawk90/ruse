//! The Ex file commands (#610): the `:w` family (`:[range]w[!] [file]`, `:w >>`, `:w !cmd`, `:up`, `:wq`,
//! `:x`), `:sav`, `:wa` and `:e[!] [++ff] [+cmd] [file]`. Every replacing write goes through
//! [`persist::atomic::write`] (temp file, fsync, rename, directory fsync; in place for a hard-linked file or
//! one a rename would strip; a FIFO or device is refused, #907; a read-only file needs `!`, E505); only
//! `:w >>` appends, as Vim does. Errors carry nvim's E-numbers so a `|` chain stops on them.

use std::fs;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};

use ruse_core::{DocumentId, SubRange, Workspace};

use crate::app::dispatch::{
    attach_file, buffer_for_path, line_count, open_file_with, resolve_normal_range, Files,
    Highlighters,
};
use crate::input::{EditSpec, FileFormatOpt, FileOpts, WriteSpec, WriteTarget};
use crate::persist;
use crate::persist::encoding::{Eol, FileFormat};

/// `fmt` with the line ending a `++ff=` forces.
pub(crate) fn with_ff(fmt: FileFormat, opts: FileOpts) -> FileFormat {
    match opts.ff {
        Some(FileFormatOpt::Unix) => FileFormat {
            eol: Eol::Lf,
            ..fmt
        },
        Some(FileFormatOpt::Dos) => FileFormat {
            eol: Eol::Crlf,
            ..fmt
        },
        None => fmt,
    }
}

/// The buffer text to write (with the final newline `'endofline'` or `:set fixeol` adds) and its on-disk
/// bytes.
pub(crate) fn encode(text: &[u8], fmt: FileFormat, fixeol: bool) -> (Vec<u8>, Vec<u8>) {
    let mut source = text.to_vec();
    // The newline is added before `to_disk`, so a CRLF file gets `\r\n`. A file opened without a final
    // newline keeps it missing unless `:set fixeol` (#1107).
    if (fixeol || fmt.final_newline) && !source.is_empty() && source.last() != Some(&b'\n') {
        source.push(b'\n');
    }
    let disk = fmt.to_disk(&source);
    (source, disk)
}

/// The Vim write report: `"f.txt" [New] 3L, 6B written`, with `[noeol]` when the text has no final
/// newline (ruse keeps a missing one, so it is never a silent surprise).
pub(crate) fn write_report(
    name: &str,
    new: bool,
    source: &[u8],
    disk: usize,
    verb: &str,
) -> String {
    let new_tag = if new { " [New]" } else { "" };
    let noeol = match (source.is_empty() || source.last() == Some(&b'\n'), new) {
        (true, _) => "",
        (false, true) => "[noeol]",
        (false, false) => " [noeol]",
    };
    format!(
        "\"{name}\"{new_tag}{noeol} {}L, {disk}B {verb}",
        line_count(source)
    )
}

/// Why a write could not be made. `Display` is the status line: nvim's message where it has one.
#[derive(Debug)]
pub(crate) enum FileError {
    /// The durable write itself failed (`write failed: …`, `E505`, `E514`, `E667`).
    Write(persist::atomic::SaveError),
    /// `E139`: another buffer holds the target file.
    LoadedElsewhere,
    /// `E17` (no `!`; `shown` is the absolute path) or `E502` (with `!`; the name as typed).
    Directory { shown: String, bang: bool },
    /// `E13`: the target exists and there is no `!`.
    Exists,
}

impl std::fmt::Display for FileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FileError::Write(e) => write!(f, "{e}"),
            FileError::LoadedElsewhere => f.write_str("E139: File is loaded in another buffer"),
            FileError::Directory { shown, bang: true } => {
                write!(f, "E502: \"{shown}\" is a directory")
            }
            FileError::Directory { shown, bang: false } => {
                write!(f, "E17: \"{shown}\" is a directory")
            }
            FileError::Exists => f.write_str("E13: File exists (add ! to override)"),
        }
    }
}

/// Whether `range` covers every line of the focused buffer (`:1,$w` writes the whole buffer, so it marks it
/// saved and needs no `!` on its own file, as in Vim).
fn covers_whole(ws: &Workspace, range: SubRange) -> bool {
    let total = line_count(ws.focused().doc.bytes());
    resolve_normal_range(ws, range) == (1, total)
}

/// Write `disk` to the buffer `id`'s own file and mark it saved; the status is the write report. `force`
/// (`:w!`) writes a read-only file.
#[allow(clippy::too_many_arguments)]
fn write_own(
    ws: &mut Workspace,
    id: DocumentId,
    path: &Path,
    source: &[u8],
    disk: &[u8],
    mark_saved: bool,
    force: bool,
) -> Result<String, FileError> {
    match persist::atomic::write(path, disk, force) {
        Ok(method) => {
            if mark_saved {
                if let Some(doc) = ws.document_mut(id) {
                    doc.mark_saved();
                }
                persist::journal::clear(Some(path)); // saved bytes are durable — nothing to recover
            }
            tracing::info!(event = "save", path = %path.display(), bytes = disk.len());
            Ok(write_report(
                &path.display().to_string(),
                false,
                source,
                disk.len(),
                "written",
            ) + method.note())
        }
        Err(e) => {
            // Expected external failure (§7): surface it in the status bar, log once, keep the buffer.
            tracing::warn!(event = "save.failed", path = %path.display(), error = %e);
            Err(FileError::Write(e))
        }
    }
}

/// Why a write to a file other than the buffer's own cannot start, as nvim reports it: `E139` when another
/// buffer holds it, `E17`/`E502` for a directory, `E13` for an existing file without `!`. `Ok(new)` says
/// whether the file does not exist yet.
fn check_other_target(
    files: &Files,
    id: DocumentId,
    target: &Path,
    name: &str,
    bang: bool,
) -> Result<bool, FileError> {
    if buffer_for_path(files, target).is_some_and(|other| other != id) {
        return Err(FileError::LoadedElsewhere);
    }
    match fs::metadata(target) {
        Ok(m) if m.is_dir() && bang => Err(FileError::Directory {
            shown: name.to_string(),
            bang,
        }),
        Ok(m) if m.is_dir() => {
            let abs = std::path::absolute(target).unwrap_or_else(|_| target.to_path_buf());
            Err(FileError::Directory {
                shown: abs.display().to_string(),
                bang,
            })
        }
        Ok(_) if !bang => Err(FileError::Exists),
        Ok(_) => Ok(false),
        // A dangling symlink or unreadable entry still exists for `E13`.
        Err(_) if fs::symlink_metadata(target).is_ok() && !bang => Err(FileError::Exists),
        Err(_) => Ok(true),
    }
}

/// Make `path` the focused buffer's file: its name, file identity and highlighter (`:sav`, and `:w {file}`
/// from a buffer with no file, `:file {name}`).
pub(crate) fn adopt_file(
    ws: &mut Workspace,
    files: &mut Files,
    highlighters: &mut Highlighters,
    id: DocumentId,
    name: &str,
    fmt: FileFormat,
) {
    ws.set_focused_buffer_name(name);
    attach_file(files, highlighters, id, PathBuf::from(name), fmt);
}

/// Append `bytes` to the existing regular file `path` and sync it (`:w >>`). A missing file is an error, as
/// in nvim (`E212`), and so is a FIFO or device.
fn append_to(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if !fs::metadata(path)?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "is not a regular file",
        ));
    }
    let mut f = fs::OpenOptions::new().append(true).open(path)?;
    f.write_all(bytes)?;
    f.sync_all()
}

/// Run one `:w`-family command on the focused buffer (`:help :w`). Returns whether it succeeded (a skipped
/// `:up`/`:x` write of an unmodified buffer counts), so `:wq`/`:x` know whether to close the window.
pub(crate) fn ex_write(
    spec: &WriteSpec,
    ws: &mut Workspace,
    files: &mut Files,
    highlighters: &mut Highlighters,
    fixeol: bool,
    status: &mut String,
) -> bool {
    let id = ws.focused_buffer();
    if spec.if_modified && !ws.focused().doc.is_modified() {
        return true;
    }
    let whole = spec.range.is_none_or(|r| covers_whole(ws, r));
    let text = match spec.range {
        Some(r) if !whole => ws.range_text(r).unwrap_or_default(),
        _ => ws.focused().doc.bytes().to_vec(),
    };
    let own = files.get(&id).map(|bf| (bf.path.clone(), bf.fmt));
    let fmt = with_ff(own.as_ref().map(|o| o.1).unwrap_or_default(), spec.opts);
    let result = match &spec.target {
        // `:w !cmd`: the lines go to the command's stdin; its output is shown and the buffer is untouched.
        WriteTarget::Command(cmd) => match crate::shell::filter(cmd, &text) {
            Ok(out) => {
                let shown = out.text.trim_end_matches('\n').replace('\n', " | ");
                Ok(match out.failure() {
                    Some(note) if shown.is_empty() => note,
                    Some(note) => format!("{shown} | {note}"),
                    None => shown,
                })
            }
            Err(e) => Err(e.to_string()),
        },
        WriteTarget::Append(name) => {
            let path = match (name, &own) {
                (Some(n), _) => PathBuf::from(n),
                (None, Some((p, _))) => p.clone(),
                (None, None) => {
                    *status = "E32: No file name".into();
                    return false;
                }
            };
            let (source, disk) = encode(&text, fmt, fixeol);
            match append_to(&path, &disk) {
                Ok(()) => Ok(write_report(
                    &path.display().to_string(),
                    false,
                    &source,
                    disk.len(),
                    "appended",
                )),
                Err(e) => Err(format!("E212: Can't open file for writing: {e}")),
            }
        }
        WriteTarget::File(name) => {
            let to_own = match (name, &own) {
                (None, _) => true,
                (Some(n), Some((p, _))) => {
                    buffer_for_path(files, Path::new(n)) == Some(id) || {
                        // Not open under this spelling, but the same file (`:w ./f.txt`).
                        let canon = |p: &Path| fs::canonicalize(p).ok();
                        canon(Path::new(n)).is_some_and(|c| Some(c) == canon(p))
                    }
                }
                (Some(_), None) => false,
            };
            let (source, disk) = encode(&text, fmt, fixeol);
            if to_own {
                match &own {
                    None => Err("E32: No file name".to_string()),
                    Some(_) if !whole && !spec.bang => {
                        Err("E140: Use ! to write partial buffer".to_string())
                    }
                    Some((path, _)) => write_own(ws, id, path, &source, &disk, whole, spec.bang)
                        .map_err(|e| e.to_string()),
                }
            } else {
                let name = name.as_deref().expect("a write to another file names it");
                let target = PathBuf::from(name);
                match check_other_target(files, id, &target, name, spec.bang) {
                    Err(e) => Err(e.to_string()),
                    Ok(new) => match persist::atomic::write(&target, &disk, spec.bang) {
                        Ok(method) => {
                            // A buffer with no file takes the name it is first written to (Vim's
                            // 'cpoptions' `F`); written whole, it is then saved.
                            if own.is_none() {
                                adopt_file(ws, files, highlighters, id, name, fmt);
                                if whole {
                                    ws.focused_doc_mut().mark_saved();
                                    persist::journal::clear(Some(&target));
                                }
                            }
                            Ok(write_report(name, new, &source, disk.len(), "written")
                                + method.note())
                        }
                        Err(e) => Err(e.to_string()),
                    },
                }
            }
        }
    };
    match result {
        Ok(msg) => {
            *status = msg;
            true
        }
        Err(msg) => {
            *status = msg;
            false
        }
    }
}

/// `:sav[eas][!] [++ff] {file}`: write the whole buffer to `{file}`, which becomes the buffer's file (its
/// name, highlighter and recovery journal follow it). Returns whether the write succeeded.
#[allow(clippy::too_many_arguments)]
pub(crate) fn ex_saveas(
    bang: bool,
    name: &str,
    opts: FileOpts,
    ws: &mut Workspace,
    files: &mut Files,
    highlighters: &mut Highlighters,
    fixeol: bool,
    status: &mut String,
) -> bool {
    let id = ws.focused_buffer();
    let target = PathBuf::from(name);
    let own = files.get(&id).map(|bf| (bf.path.clone(), bf.fmt));
    if buffer_for_path(files, &target) == Some(id) {
        // `:sav %` is a plain write.
        let spec = WriteSpec {
            range: None,
            bang,
            target: WriteTarget::File(None),
            opts,
            if_modified: false,
            quit: false,
        };
        return ex_write(&spec, ws, files, highlighters, fixeol, status);
    }
    let new = match check_other_target(files, id, &target, name, bang) {
        Ok(new) => new,
        Err(e) => {
            *status = e.to_string();
            return false;
        }
    };
    let fmt = with_ff(own.as_ref().map(|o| o.1).unwrap_or_default(), opts);
    let (source, disk) = encode(ws.focused().doc.bytes(), fmt, fixeol);
    match persist::atomic::write(&target, &disk, bang) {
        Ok(method) => {
            if let Some((old, _)) = &own {
                persist::journal::clear(Some(old)); // the changes are on disk under the new name
            }
            adopt_file(ws, files, highlighters, id, name, fmt);
            ws.focused_doc_mut().mark_saved();
            persist::journal::clear(Some(&target));
            *status = write_report(name, new, &source, disk.len(), "written") + method.note();
            true
        }
        Err(e) => {
            *status = e.to_string();
            false
        }
    }
}

/// `:wa[ll]`: write every modified buffer that has a file (`skip` leaves some out: a terminal). A modified
/// buffer with no file is `E141`, and the others are still written, as in nvim. Returns whether every
/// modified buffer reached disk.
pub(crate) fn ex_write_all(
    ws: &mut Workspace,
    files: &Files,
    fixeol: bool,
    status: &mut String,
    skip: impl Fn(DocumentId) -> bool,
) -> bool {
    let mut error = None;
    let mut last = None;
    for b in ws.buffers() {
        if !b.modified || skip(b.id) {
            continue;
        }
        let Some(bf) = files.get(&b.id) else {
            error = Some(format!("E141: No file name for buffer {}", b.id.0));
            continue;
        };
        let Some(text) = ws.document(b.id).map(|d| d.bytes().to_vec()) else {
            continue;
        };
        let (source, disk) = encode(&text, bf.fmt, fixeol);
        match write_own(ws, b.id, &bf.path, &source, &disk, true, false) {
            Ok(msg) => last = Some(msg),
            Err(e) => error = Some(e.to_string()),
        }
    }
    match (error, last) {
        (Some(e), _) => {
            *status = e.to_string();
            false
        }
        (None, Some(msg)) => {
            *status = msg;
            true
        }
        (None, None) => true,
    }
}

/// Re-read the focused buffer's file from disk (`:e`, `:e!`, `:e %`), replacing its text; `++ff=` forces
/// the line ending it is read with. Returns the status line.
pub(crate) fn reload_focused_file(ws: &mut Workspace, files: &mut Files, opts: FileOpts) -> String {
    let id = ws.focused_buffer();
    let Some(path) = files.get(&id).map(|bf| bf.path.clone()) else {
        return "E32: No file name".into();
    };
    match persist::read_file(&path) {
        Ok(raw) => {
            let fmt = with_ff(FileFormat::detect(&raw), opts);
            ws.reload_focused(fmt.to_buffer(&raw));
            // The changes were thrown away on purpose (`:e!`); a journal of them must not be offered on
            // the next start (#862). Neovim's swap file then holds the re-read text, which offers nothing.
            persist::journal::discard(Some(&path));
            // The path is unchanged, so the buffer keeps its file entry and its highlighter: the reload
            // continues the buffer's revisions (#687), and the highlighter reparses on a new revision
            // against the bytes it last saw, so it never shows the pre-reload text (#894).
            if let Some(bf) = files.get_mut(&id) {
                bf.fmt = fmt;
            }
            format!("\"{}\" reloaded", path.display())
        }
        Err(e) => format!("E484: can't open {}: {e}", path.display()),
    }
}

/// `:e[!] [++ff] [{file}]`: open `{file}` (a file that does not exist yet opens as a new, empty buffer), or
/// re-read the focused buffer's own file, which its unsaved changes block without `!` (`E37`). Returns
/// whether a buffer was opened or re-read, so a `+cmd` knows whether to run.
pub(crate) fn ex_edit(
    spec: &EditSpec,
    ws: &mut Workspace,
    files: &mut Files,
    highlighters: &mut Highlighters,
    status: &mut String,
) -> bool {
    let focused = ws.focused_buffer();
    let own = spec
        .file
        .as_deref()
        .is_none_or(|f| buffer_for_path(files, Path::new(f)) == Some(focused));
    if !own {
        let file = spec.file.as_deref().expect("another file is named");
        let opened = open_file_with(file, ws, files, highlighters, |fmt| with_ff(fmt, spec.opts));
        *status = opened.status;
        return opened.id.is_some();
    }
    if !files.contains_key(&focused) {
        *status = "E32: No file name".into();
        false
    } else if ws.focused().doc.is_modified() && !spec.bang {
        *status = "E37: No write since last change (add ! to override)".into();
        false
    } else {
        *status = reload_focused_file(ws, files, spec.opts);
        !status.starts_with('E')
    }
}

/// The Ex line a `:e +cmd` runs once the file is open: `+` alone is `$` (the last line), anything else
/// (`+5`, `+/pat`, `+set ts=8`) runs as typed.
pub(crate) fn plus_cmd_line(cmd: &str) -> String {
    if cmd.is_empty() {
        "$".into()
    } else {
        cmd.to_string()
    }
}
