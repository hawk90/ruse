//! Frontend helpers for the Ex commands added in #609/#610 that the single dispatch
//! (`Session::execute_ex_one`) calls: showing print output, `:filter`, `:put!`, `:find`, `:cd` and the
//! alternate file name a write or rename leaves behind.

use std::path::{Path, PathBuf};

use ruse_core::{DocumentId, LineAddr, Workspace};

use crate::app::dispatch::Files;
use crate::ui::overlay::Overlay;

/// The alternate file name (`#`) a `:w {file}`, `:sav` or `:file {name}` set (#610). nvim gives the name
/// an (unlisted) buffer; ruse keeps it here, and it holds while the focused and alternate buffers are the
/// ones it was set under, so switching buffers replaces it as it would replace nvim's.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AltFile {
    pub(crate) focused: DocumentId,
    pub(crate) alternate: Option<DocumentId>,
    pub(crate) name: String,
}

impl AltFile {
    /// An alternate file name for the current focus of `ws`.
    pub(crate) fn new(ws: &Workspace, name: String) -> AltFile {
        AltFile {
            focused: ws.focused_buffer(),
            alternate: ws.alternate(),
            name,
        }
    }

    /// The name, while it still applies to `ws`.
    pub(crate) fn current<'a>(this: &'a Option<AltFile>, ws: &Workspace) -> Option<&'a str> {
        this.as_ref()
            .filter(|a| a.focused == ws.focused_buffer() && a.alternate == ws.alternate())
            .map(|a| a.name.as_str())
    }
}

/// The alternate file name `#` stands for: the one a write or rename set, else the alternate buffer's file.
pub(crate) fn alternate_name(
    alt: &Option<AltFile>,
    ws: &Workspace,
    files: &Files,
) -> Option<String> {
    AltFile::current(alt, ws).map(str::to_string).or_else(|| {
        ws.alternate()
            .and_then(|id| files.get(&id))
            .map(|bf| bf.path.display().to_string())
    })
}

/// Show print output (#609): nothing for no rows, one row on the status line, more in the view-only
/// listing overlay (the message area ruse has), with the last row on the status line.
pub(crate) fn show_rows(
    rows: Vec<String>,
    status: &mut crate::app::messages::Status,
    listing: &mut Option<Overlay>,
) {
    match rows.len() {
        0 => {}
        1 => status.echo(rows.into_iter().next().unwrap_or_default()),
        _ => {
            *listing = Some(Overlay::Listing(
                crate::ui::keyword_lines_picker::open_rows(&rows),
            ));
            status.echo(rows.last().cloned().unwrap_or_default());
        }
    }
}

/// A line test: whether a print command shows a line.
pub(crate) type LineTest = Box<dyn Fn(&str) -> bool>;

/// The line test a `:filter` asks for (`:help :filter`): the lines that match `pat` (`!`: that do not),
/// compiled with the focused view's search case. No filter keeps every line.
///
/// # Errors
/// The pattern's error when it does not compile.
pub(crate) fn filter_keep(
    ws: &Workspace,
    filter: Option<&(String, bool)>,
) -> Result<LineTest, ruse_core::RegexError> {
    let Some((pat, bang)) = filter else {
        return Ok(Box::new(|_| true));
    };
    if pat.is_empty() {
        return Ok(Box::new(|_| true));
    }
    let re = ruse_core::pattern::Regex::compile(pat, ws.focused().view.search_options())?;
    let bang = *bang;
    Ok(Box::new(move |line: &str| {
        re.find_at(line, ruse_core::BytePos(0)).is_some() != bang
    }))
}

/// `:put!` puts above the addressed line: after the line before it.
pub(crate) fn put_above(addr: LineAddr) -> LineAddr {
    match addr {
        LineAddr::Line(n) => LineAddr::Line(n.saturating_sub(1)),
        LineAddr::Current => LineAddr::CurrentOffset(-1),
        LineAddr::CurrentOffset(d) => LineAddr::CurrentOffset(d - 1),
        LineAddr::Last => LineAddr::LastOffset(-1),
        LineAddr::LastOffset(d) => LineAddr::LastOffset(d - 1),
    }
}

/// The byte offset where 1-based line `line` of `bytes` starts (the end for a line past the last).
pub(crate) fn line_start(bytes: &[u8], line: usize) -> usize {
    bytes
        .split_inclusive(|&b| b == b'\n')
        .take(line.saturating_sub(1))
        .map(<[u8]>::len)
        .sum::<usize>()
        .min(bytes.len())
}

/// Where `:find {name}` finds a file with the default `'path'` of `.,,` (#610): an absolute name, or one
/// starting with `./` or `../`, as it is; otherwise in the directory of the current file (`current`), then
/// in the working directory. `None` when it is not there.
pub(crate) fn find_in_path(name: &str, current: Option<&Path>) -> Option<String> {
    let is_file = |p: &Path| p.is_file();
    let path = Path::new(name);
    if path.is_absolute() || name.starts_with("./") || name.starts_with("../") {
        return is_file(path).then(|| name.to_string());
    }
    if let Some(dir) = current
        .and_then(Path::parent)
        .filter(|d| !d.as_os_str().is_empty())
    {
        let cand = dir.join(name);
        if is_file(&cand) {
            return Some(cand.display().to_string());
        }
    }
    is_file(path).then(|| name.to_string())
}

/// Why a `:cd` could not be made. `Display` is nvim's message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum CdError {
    /// `:cd -` before any `:cd` (`E186`).
    NoPrevious,
    /// The directory cannot be entered (`E344`); the name as typed.
    NotFound(String),
    /// The working directory itself cannot be read.
    Cwd(String),
}

impl std::fmt::Display for CdError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CdError::NoPrevious => f.write_str("E186: No previous directory"),
            CdError::NotFound(d) => write!(f, "E344: Can't find directory \"{d}\" in cdpath"),
            CdError::Cwd(e) => write!(f, "E: cannot read the working directory: {e}"),
        }
    }
}

/// The directory `:cd {arg}` goes to (#610): the home directory without an argument, `prev` for `-`, and
/// `~` at the start stands for the home directory.
///
/// # Errors
/// [`CdError::NoPrevious`] for `-` with no previous directory.
pub(crate) fn cd_target(arg: Option<&str>, prev: Option<&Path>) -> Result<PathBuf, CdError> {
    let home = || {
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default()
    };
    Ok(match arg {
        None => home(),
        Some("-") => prev.map(Path::to_path_buf).ok_or(CdError::NoPrevious)?,
        Some("~") => home(),
        Some(d) => match d.strip_prefix("~/") {
            Some(rest) => home().join(rest),
            None => PathBuf::from(d),
        },
    })
}

/// Change the working directory to `target` (#610) and keep every buffer's file pointing at the same file:
/// a path is made absolute against the old directory, then written relative to the new one when it lies
/// under it (as nvim shows names). Returns the old directory.
///
/// # Errors
/// [`CdError::NotFound`] when the directory cannot be entered.
pub(crate) fn change_dir(
    target: &Path,
    shown: &str,
    ws: &mut Workspace,
    files: &mut Files,
) -> Result<PathBuf, CdError> {
    let old = std::env::current_dir().map_err(|e| CdError::Cwd(e.to_string()))?;
    std::env::set_current_dir(target).map_err(|_| CdError::NotFound(shown.to_string()))?;
    let new = std::env::current_dir().unwrap_or_else(|_| target.to_path_buf());
    let focused = ws.focused_buffer();
    let mut renamed: Vec<(DocumentId, String)> = Vec::new();
    for (id, bf) in files.iter_mut() {
        let abs = if bf.path.is_absolute() {
            bf.path.clone()
        } else {
            old.join(&bf.path)
        };
        let rel = abs
            .strip_prefix(&new)
            .map_or_else(|_| abs.clone(), Path::to_path_buf);
        let rel = if rel.as_os_str().is_empty() {
            abs.clone()
        } else {
            rel
        };
        if rel != bf.path {
            bf.path = rel;
            renamed.push((*id, bf.path.display().to_string()));
        }
    }
    // Buffer names follow their files; only the focused buffer can be renamed in place.
    for (id, name) in renamed {
        if id == focused {
            ws.set_focused_buffer_name(name);
        } else {
            ws.set_buffer_name(id, name);
        }
    }
    Ok(old)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn put_above_is_after_the_line_before() {
        assert_eq!(put_above(LineAddr::Line(2)), LineAddr::Line(1));
        assert_eq!(put_above(LineAddr::Line(0)), LineAddr::Line(0));
        assert_eq!(put_above(LineAddr::Current), LineAddr::CurrentOffset(-1));
        assert_eq!(put_above(LineAddr::Last), LineAddr::LastOffset(-1));
    }

    #[test]
    fn cd_targets() {
        let prev = PathBuf::from("/tmp");
        assert_eq!(cd_target(Some("-"), Some(&prev)), Ok(prev.clone()));
        assert_eq!(cd_target(Some("-"), None), Err(CdError::NoPrevious));
        assert_eq!(cd_target(Some("sub"), None), Ok(PathBuf::from("sub")));
    }

    #[test]
    fn line_starts() {
        assert_eq!(line_start(b"ab\ncd\n", 1), 0);
        assert_eq!(line_start(b"ab\ncd\n", 2), 3);
        assert_eq!(line_start(b"ab\ncd\n", 9), 6);
    }
}
