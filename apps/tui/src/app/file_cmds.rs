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
use crate::app::messages::{Msg, Status};
use crate::app::write_opts::{write_file, WriteOptions};
use crate::input::{EditSpec, FileFormatOpt, FileOpts, WriteSpec, WriteTarget};
use crate::persist;
use crate::persist::encoding::{Eol, FileFormat};

/// `fmt` with the line ending a `++ff=` forces and the `'binary'` a `++bin` / `++nobin` sets.
pub(crate) fn with_ff(fmt: FileFormat, opts: FileOpts) -> FileFormat {
    let fmt = match opts.bin {
        // `++bin` reads with no conversion: the line ending and BOM detection do not apply.
        Some(true) => FileFormat {
            eol: Eol::Lf,
            bom: false,
            eof: false,
            binary: true,
            ..fmt
        },
        Some(false) => FileFormat {
            binary: false,
            ..fmt
        },
        None => fmt,
    };
    match opts.ff {
        Some(FileFormatOpt::Mac) => FileFormat {
            eol: Eol::Cr,
            ..fmt
        },
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

/// The Vim write report: `"f.txt" [New] 3L, 6B written`, with `[noeol]` when the text has no final
/// newline (ruse keeps a missing one, so it is never a silent surprise).
pub(crate) fn write_report(
    name: &str,
    new: bool,
    written: &crate::app::write_opts::Encoded,
    verb: &str,
    fmt: FileFormat,
) -> String {
    let (source, disk) = (&written.source[..], written.disk.len());
    // nvim's `[New][noeol][dos]` (`msg_add_fileformat`): the tags follow each other after one blank; a
    // binary write uses no fileformat.
    let mut tags = String::new();
    if new {
        tags.push_str("[New]");
    }
    if written.noeol {
        tags.push_str("[noeol]");
    }
    if !fmt.binary && fmt.eol != Eol::Lf {
        tags.push_str(&format!("[{}]", fmt.eol.name()));
    }
    let tags = if tags.is_empty() {
        tags
    } else {
        format!(" {tags}")
    };
    // nvim counts the emptied buffer as no lines (`0L, 0B`), though it shows one empty line.
    let lines = if source.is_empty() {
        0
    } else {
        line_count(source)
    };
    format!("\"{name}\"{tags} {lines}L, {disk}B {verb}")
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
    /// The file changed on disk since ruse read or saved it, and there is no `!` (RFC-0023 §8, #938).
    ChangedOnDisk,
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
            FileError::ChangedOnDisk => f.write_str(persist::disk::CHANGED_SINCE_READ),
        }
    }
}

/// Whether `range` covers every line of the focused buffer (`:1,$w` writes the whole buffer, so it marks it
/// saved and needs no `!` on its own file, as in Vim).
fn covers_whole(ws: &Workspace, range: SubRange) -> bool {
    let total = line_count(ws.focused().doc.bytes());
    resolve_normal_range(ws, range) == (1, total)
}

/// Whether `range` includes the focused buffer's last line, so its text ends as the file does (RFC-0024 §3:
/// a range that stops earlier is whole lines, each with its newline).
pub(crate) fn reaches_last_line(ws: &Workspace, range: SubRange) -> bool {
    resolve_normal_range(ws, range).1 >= line_count(ws.focused().doc.bytes())
}

/// The format a write of buffer `id` uses: its file's, else the one `:w` would give a buffer with no file.
pub(crate) fn buffer_format(files: &Files, id: DocumentId) -> FileFormat {
    files
        .get(&id)
        .map(|bf| bf.fmt)
        .or_else(|| files.write.nofile_fmt.get(&id).copied())
        .unwrap_or_else(|| files.write.new_format())
}

/// Write `disk` to the buffer `id`'s own file and mark it saved; the status is the write report. `force`
/// (`:w!`) writes a read-only file.
#[allow(clippy::too_many_arguments)]
fn write_own(
    ws: &mut Workspace,
    id: DocumentId,
    path: &Path,
    enc: &crate::app::write_opts::Encoded,
    mark_saved: bool,
    force: bool,
    fmt: FileFormat,
    w: &WriteOptions,
) -> Result<String, FileError> {
    let disk = &enc.disk[..];
    // A file changed by another program since ruse read or saved it is not overwritten without `!`.
    if !force && persist::disk::conflicts(path) {
        return Err(FileError::ChangedOnDisk);
    }
    match write_file(w, path, disk, force) {
        Ok(written) => {
            persist::disk::saw(path, disk);
            if mark_saved {
                if let Some(doc) = ws.document_mut(id) {
                    doc.mark_saved();
                }
                persist::journal::clear(Some(path)); // saved bytes are durable — nothing to recover
            }
            crate::log::save(path, disk.len());
            Ok(
                write_report(&path.display().to_string(), false, enc, "written", fmt)
                    + &written.note(),
            )
        }
        Err(e) => {
            // Expected external failure (§7): surface it in the status bar, log once, keep the buffer.
            crate::log::save_failed(path, &e);
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
    writeany: bool,
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
        // `'writeany'` writes over an existing file without `!` (nvim: not a read-only one, E505).
        Ok(_) if !bang && !writeany => Err(FileError::Exists),
        Ok(_) => Ok(false),
        // A dangling symlink or unreadable entry still exists for `E13`.
        Err(_) if fs::symlink_metadata(target).is_ok() && !bang && !writeany => {
            Err(FileError::Exists)
        }
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
    let len = ws.document(id).map_or(0, |d| d.len());
    crate::app::dispatch::mark_large(files, highlighters, id, len);
}

/// Whether a `:w`-family file argument names the buffer `id`'s own file `own`: no argument, the name it is
/// open under, or another spelling of the same file (`:w ./f.txt`).
fn names_own(files: &Files, id: DocumentId, name: Option<&str>, own: Option<&Path>) -> bool {
    let Some(own) = own else {
        return false;
    };
    let Some(n) = name else {
        return true;
    };
    let canon = |p: &Path| fs::canonicalize(p).ok();
    buffer_for_path(files, Path::new(n)) == Some(id)
        || canon(Path::new(n)).is_some_and(|c| Some(c) == canon(own))
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

/// Why a `:w`-family command failed. `Display` is the status line: nvim's message where it has one.
#[derive(Debug)]
enum WriteFailure {
    /// `E32`: no file argument and the buffer has no file.
    NoFileName,
    /// `E45`: the buffer is 'readonly' and there is no `!`.
    ReadOnly,
    /// `E140`: a range that is not the whole buffer, to its own file, without `!`.
    Partial,
    /// `E212`: the `:w >>` append failed.
    Append(io::Error),
    /// The `:w !cmd` command could not be run.
    Shell(crate::shell::ShellError),
    /// The target check or the durable write failed.
    File(FileError),
}

impl std::fmt::Display for WriteFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WriteFailure::NoFileName => f.write_str("E32: No file name"),
            WriteFailure::ReadOnly => f.write_str(crate::app::dispatch::E45),
            WriteFailure::Partial => f.write_str("E140: Use ! to write partial buffer"),
            WriteFailure::Append(e) => write!(f, "E212: Can't open file for writing: {e}"),
            WriteFailure::Shell(e) => write!(f, "{e}"),
            WriteFailure::File(e) => write!(f, "{e}"),
        }
    }
}

/// What a `:w`-family command writes, resolved once before the write (target resolution): the focused
/// buffer, the text the range selects, the buffer's own file and the format the write uses.
struct WriteSource {
    id: DocumentId,
    /// The range covers the whole buffer (or there is none): the write marks the buffer saved.
    whole: bool,
    /// The range reaches the last line, so the text ends as the file does (RFC-0024 §3).
    to_end: bool,
    text: Vec<u8>,
    /// The buffer's own file and its format.
    own: Option<(PathBuf, FileFormat)>,
    read_only: bool,
    fmt: FileFormat,
}

impl WriteSource {
    fn resolve(spec: &WriteSpec, ws: &mut Workspace, files: &Files, w: &WriteOptions) -> Self {
        let id = ws.focused_buffer();
        let whole = spec.range.is_none_or(|r| covers_whole(ws, r));
        let to_end = spec.range.is_none_or(|r| reaches_last_line(ws, r));
        let text = match spec.range {
            Some(r) if !whole => ws.range_text(r).unwrap_or_default(),
            _ => ws.focused().doc.bytes().to_vec(),
        };
        let own = files.get(&id).map(|bf| (bf.path.clone(), bf.fmt));
        let read_only = files.get(&id).is_some_and(|bf| bf.read_only);
        let fmt = with_ff(
            own.as_ref()
                .map(|o| o.1)
                .or_else(|| w.nofile_fmt.get(&id).copied())
                .unwrap_or_else(|| w.new_format()),
            spec.opts,
        );
        WriteSource {
            id,
            whole,
            to_end,
            text,
            own,
            read_only,
            fmt,
        }
    }

    /// The bytes a file write puts on disk: the text as the file ends when it reaches the last line, else
    /// whole lines (RFC-0024 §3).
    fn encode(&self, w: &WriteOptions) -> crate::app::write_opts::Encoded {
        if self.to_end {
            w.encode(&self.text, self.fmt)
        } else {
            w.encode_lines(&self.text, self.fmt)
        }
    }

    /// Whether the file argument `name` names this buffer's own file.
    fn names_own(&self, files: &Files, name: Option<&str>) -> bool {
        names_own(
            files,
            self.id,
            name,
            self.own.as_ref().map(|o| o.0.as_path()),
        )
    }
}

/// Run one `:w`-family command on the focused buffer (`:help :w`). Returns whether it succeeded (a skipped
/// `:up`/`:x` write of an unmodified buffer counts), so `:wq`/`:x` know whether to close the window.
pub(crate) fn ex_write(
    spec: &WriteSpec,
    ws: &mut Workspace,
    files: &mut Files,
    highlighters: &mut Highlighters,
    status: &mut Status,
) -> bool {
    let w = &files.write.clone();
    if spec.if_modified && !ws.focused().doc.is_modified() {
        return true;
    }
    if !matches!(spec.target, WriteTarget::Command(_)) {
        if let Err(e) = w.check_write() {
            status.error(e);
            return false;
        }
    }
    let src = WriteSource::resolve(spec, ws, files, w);
    let result = match &spec.target {
        WriteTarget::Command(cmd) => write_command(cmd, &src, w),
        WriteTarget::Append(name) => write_append(name.as_deref(), &src, files, w),
        WriteTarget::File(name) => {
            write_to_file(spec, name.as_deref(), &src, ws, files, highlighters, w)
        }
    };
    match result {
        Ok(msg) => {
            status.echo(msg);
            true
        }
        Err(e) => {
            status.error(e.to_string());
            false
        }
    }
}

/// `:w !cmd`: the lines go to the command's stdin; its output is shown and the buffer is untouched. A range
/// that reaches the last line ends as the file does (nvim: `:w !od -c` of `a\nb` without `'eol'` sends three
/// bytes, RFC-0024 §3).
fn write_command(cmd: &str, src: &WriteSource, w: &WriteOptions) -> Result<String, WriteFailure> {
    match crate::shell::filter(
        cmd,
        if src.to_end {
            w.lines_out(&src.text, src.fmt)
        } else {
            &src.text
        },
    ) {
        Ok(out) => {
            let shown = out.text.trim_end_matches('\n').replace('\n', " | ");
            Ok(match out.failure() {
                Some(note) if shown.is_empty() => note,
                Some(note) => format!("{shown} | {note}"),
                None => shown,
            })
        }
        Err(e) => Err(WriteFailure::Shell(e)),
    }
}

/// `:w >> [file]`: append to `name`, else the buffer's own file (`E32` with neither).
fn write_append(
    name: Option<&str>,
    src: &WriteSource,
    files: &Files,
    w: &WriteOptions,
) -> Result<String, WriteFailure> {
    let path = match (name, &src.own) {
        (Some(n), _) => PathBuf::from(n),
        (None, Some((p, _))) => p.clone(),
        (None, None) => return Err(WriteFailure::NoFileName),
    };
    let enc = src.encode(w);
    // An append to the buffer's own file is ruse's own write, not a change on disk (#1221).
    let before = src
        .names_own(files, name)
        .then(|| persist::disk::check(&path));
    match append_to(&path, &enc.disk) {
        Ok(()) => {
            if let Some(before) = before {
                persist::disk::appended(&path, before);
            }
            Ok(write_report(
                &path.display().to_string(),
                false,
                &enc,
                "appended",
                src.fmt,
            ))
        }
        Err(e) => Err(WriteFailure::Append(e)),
    }
}

/// `:[range]w[!] [file]`: a replacing write, to the buffer's own file or to another one.
fn write_to_file(
    spec: &WriteSpec,
    name: Option<&str>,
    src: &WriteSource,
    ws: &mut Workspace,
    files: &mut Files,
    highlighters: &mut Highlighters,
    w: &WriteOptions,
) -> Result<String, WriteFailure> {
    let to_own = name.is_none() || src.names_own(files, name);
    let enc = src.encode(w);
    if to_own {
        write_to_own(spec, src, &enc, ws, files, w)
    } else {
        let name = name.expect("a write to another file names it");
        write_to_other(spec, name, src, &enc, ws, files, highlighters, w)
    }
}

/// The write to the buffer's own file: `E32` with none, `E45` for a 'readonly' one and `E140` for part of the
/// buffer without `!`.
fn write_to_own(
    spec: &WriteSpec,
    src: &WriteSource,
    enc: &crate::app::write_opts::Encoded,
    ws: &mut Workspace,
    files: &mut Files,
    w: &WriteOptions,
) -> Result<String, WriteFailure> {
    match &src.own {
        None => Err(WriteFailure::NoFileName),
        Some(_) if src.read_only && !spec.bang => Err(WriteFailure::ReadOnly),
        Some(_) if !src.whole && !spec.bang => Err(WriteFailure::Partial),
        Some((path, _)) => {
            let written = write_own(ws, src.id, path, enc, src.whole, spec.bang, src.fmt, w)
                .map_err(WriteFailure::File);
            // `:w!` over a 'readonly' buffer's own file resets 'readonly' (nvim, without 'cpoptions' `Z`).
            if written.is_ok() && spec.bang {
                if let Some(bf) = files.get_mut(&src.id) {
                    bf.read_only = false;
                }
            }
            written
        }
    }
}

/// The write to a file other than the buffer's own, after [`check_other_target`]. A buffer with no file
/// takes the name it is first written to.
#[allow(clippy::too_many_arguments)]
fn write_to_other(
    spec: &WriteSpec,
    name: &str,
    src: &WriteSource,
    enc: &crate::app::write_opts::Encoded,
    ws: &mut Workspace,
    files: &mut Files,
    highlighters: &mut Highlighters,
    w: &WriteOptions,
) -> Result<String, WriteFailure> {
    let disk = &enc.disk;
    let target = PathBuf::from(name);
    match check_other_target(files, src.id, &target, name, spec.bang, w.writeany) {
        Err(e) => Err(WriteFailure::File(e)),
        Ok(new) => match write_file(w, &target, disk, spec.bang) {
            Ok(written) => {
                // A buffer with no file takes the name it is first written to (Vim's 'cpoptions' `F`);
                // written whole, it is then saved.
                if src.own.is_none() {
                    persist::disk::saw(&target, disk);
                    adopt_file(ws, files, highlighters, src.id, name, src.fmt);
                    if src.whole {
                        ws.focused_doc_mut().mark_saved();
                        persist::journal::clear(Some(&target));
                    }
                }
                Ok(write_report(name, new, enc, "written", src.fmt) + &written.note())
            }
            Err(e) => Err(WriteFailure::File(FileError::Write(e))),
        },
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
    status: &mut Status,
) -> bool {
    let w = &files.write.clone();
    let id = ws.focused_buffer();
    if let Err(e) = w.check_write() {
        status.error(e);
        return false;
    }
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
        return ex_write(&spec, ws, files, highlighters, status);
    }
    let new = match check_other_target(files, id, &target, name, bang, w.writeany) {
        Ok(new) => new,
        Err(e) => {
            status.error(e.to_string());
            return false;
        }
    };
    let fmt = with_ff(
        own.as_ref()
            .map(|o| o.1)
            .or_else(|| w.nofile_fmt.get(&id).copied())
            .unwrap_or_else(|| w.new_format()),
        opts,
    );
    let enc = w.encode(ws.focused().doc.bytes(), fmt);
    let disk = &enc.disk;
    match write_file(w, &target, disk, bang) {
        Ok(written) => {
            persist::disk::saw(&target, disk);
            if let Some((old, _)) = &own {
                persist::journal::clear(Some(old)); // the changes are on disk under the new name
            }
            adopt_file(ws, files, highlighters, id, name, fmt);
            ws.focused_doc_mut().mark_saved();
            persist::journal::clear(Some(&target));
            status.echo(write_report(name, new, &enc, "written", fmt) + &written.note());
            true
        }
        Err(e) => {
            status.error(e.to_string());
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
    status: &mut Status,
    skip: impl Fn(DocumentId) -> bool,
) -> bool {
    let w = &files.write;
    if let Err(e) = w.check_write() {
        if ws.buffers().iter().any(|b| b.modified && !skip(b.id)) {
            status.error(e);
            return false;
        }
    }
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
        if bf.read_only {
            error = Some(crate::app::dispatch::E45.to_string());
            continue;
        }
        let Some(text) = ws.document(b.id).map(|d| d.bytes().to_vec()) else {
            continue;
        };
        let enc = w.encode(&text, bf.fmt);
        match write_own(ws, b.id, &bf.path, &enc, true, false, bf.fmt, w) {
            Ok(msg) => last = Some(msg),
            Err(e) => error = Some(e.to_string()),
        }
    }
    match (error, last) {
        (Some(e), _) => {
            status.error(e.to_string());
            false
        }
        (None, Some(msg)) => {
            status.echo(msg);
            true
        }
        (None, None) => true,
    }
}

/// Re-read the focused buffer's file from disk (`:e`, `:e!`, `:e %`), replacing its text; `++ff=` forces
/// the line ending it is read with. Returns the message to give.
pub(crate) fn reload_focused_file(ws: &mut Workspace, files: &mut Files, opts: FileOpts) -> Msg {
    let id = ws.focused_buffer();
    let Some(path) = files.get(&id).map(|bf| bf.path.clone()) else {
        return Msg::error("E32: No file name");
    };
    match persist::read_file(&path) {
        Ok(raw) => {
            persist::disk::saw(&path, &raw);
            persist::backup::visited(&path);
            let fmt = with_ff(files.write.read_format(&raw), opts).reading(&raw);
            ws.reload_focused(fmt.to_buffer(&raw));
            // The changes were thrown away on purpose (`:e!`); a journal of them must not be offered on
            // the next start (#862). Neovim's swap file then holds the re-read text, which offers nothing.
            persist::journal::discard(Some(&path));
            // The path is unchanged, so the buffer keeps its file entry and its highlighter: the reload
            // continues the buffer's revisions (#687), and the highlighter reparses on a new revision
            // against the bytes it last saw, so it never shows the pre-reload text (#894).
            if let Some(bf) = files.get_mut(&id) {
                bf.fmt = fmt;
                bf.reloads = bf.reloads.saturating_add(1);
            }
            Msg::echo(format!("\"{}\" reloaded", path.display()))
        }
        Err(e) => {
            crate::log::open_failed("reload", &path, &e);
            Msg::error(format!("E484: can't open {}: {e}", path.display()))
        }
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
    status: &mut Status,
) -> bool {
    let focused = ws.focused_buffer();
    let own = spec
        .file
        .as_deref()
        .is_none_or(|f| buffer_for_path(files, Path::new(f)) == Some(focused));
    if !own {
        let file = spec.file.as_deref().expect("another file is named");
        let opened = open_file_with(file, ws, files, highlighters, |fmt| with_ff(fmt, spec.opts));
        status.give(opened.status);
        return opened.id.is_some();
    }
    if !files.contains_key(&focused) {
        status.error("E32: No file name");
        false
    } else if ws.focused().doc.is_modified() && !spec.bang {
        status.error("E37: No write since last change (add ! to override)");
        false
    } else {
        status.give(reload_focused_file(ws, files, spec.opts));
        !status.is_error()
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
