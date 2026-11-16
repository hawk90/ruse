//! `!`, `%` and `#` expansion with the `:p:h:t:r:e` filename modifiers. Split from `ex.rs` (#1385).

/// Why a cmdline-special expansion ([`expand_bang`], [`expand_filename`]) failed. `Display` is nvim's
/// message, shown on the status line.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ExpandError {
    /// A `!` with no previous shell command (`E34`).
    NoPreviousCommand,
    /// A `%` (or `#`) that names no file (`E499`).
    EmptyFileName,
    /// A `#` with no alternate file (`E194`).
    NoAlternateFile,
}

impl std::fmt::Display for ExpandError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            ExpandError::NoPreviousCommand => "E34: No previous command",
            ExpandError::EmptyFileName => {
                "E499: Empty file name for '%' or '#', only works with \":p:h\""
            }
            ExpandError::NoAlternateFile => "E194: No alternate file name to substitute for '#'",
        })
    }
}

/// Expand the `!` in a shell command (`:!`, `:{range}!`, `:r !`): each unescaped `!` becomes the previous
/// shell command (`:help :!`), so `:!!` repeats it; `\!` is a literal `!`. With no previous command a `!`
/// is `E34`.
///
/// # Errors
/// [`ExpandError::NoPreviousCommand`] when `cmd` holds an unescaped `!` and `prev` is `None`.
pub fn expand_bang(cmd: &str, prev: Option<&str>) -> Result<String, ExpandError> {
    let mut out = String::with_capacity(cmd.len());
    let mut chars = cmd.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' if chars.peek() == Some(&'!') => {
                out.push('!');
                chars.next();
            }
            '!' => out.push_str(prev.ok_or(ExpandError::NoPreviousCommand)?),
            _ => out.push(c),
        }
    }
    Ok(out)
}

/// The file names a cmdline-special character stands for ([`expand_filename`]).
#[derive(Clone, Copy, Default, Debug)]
pub struct FileNames<'a> {
    /// `%`: the focused buffer's file.
    pub current: Option<&'a str>,
    /// `#`: the alternate buffer's file.
    pub alternate: Option<&'a str>,
    /// `#{n}`: buffer `n`'s file, for every buffer that has one.
    pub buffers: &'a [(u64, String)],
}

/// Expand the cmdline-special file names in a file argument or shell command (`:help cmdline-special`):
/// `%` is the current file name, `#` the alternate one and `#{n}` buffer `n`'s; `\%` and `\#` are literal.
/// Each may be followed by file-name modifiers (`:help filename-modifiers`), applied left to right: `:p`
/// (full path), `:~` (relative to the home directory), `:.` (relative to the working directory), `:h`
/// (head), `:t` (tail), `:r` (root) and `:e` (extension; `:e:e` takes two). Other modifiers (`:s`, `:gs`,
/// `:S`) are not modeled and stay as text.
///
/// # Errors
/// [`ExpandError::EmptyFileName`] when `%` names no file, [`ExpandError::NoAlternateFile`] when there is no
/// alternate file for `#` or no buffer `n` with a file for `#{n}`.
pub fn expand_filename(arg: &str, names: &FileNames<'_>) -> Result<String, ExpandError> {
    let mut out = String::with_capacity(arg.len());
    let mut rest = arg;
    while let Some(c) = rest.chars().next() {
        rest = &rest[c.len_utf8()..];
        let name = match c {
            '\\' if rest.starts_with(['%', '#']) => {
                out.push_str(&rest[..1]);
                rest = &rest[1..];
                continue;
            }
            '%' => names.current.ok_or(ExpandError::EmptyFileName)?.to_string(),
            '#' => {
                let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
                if digits == 0 {
                    names
                        .alternate
                        .ok_or(ExpandError::NoAlternateFile)?
                        .to_string()
                } else {
                    let n: Option<u64> = rest[..digits].parse().ok();
                    rest = &rest[digits..];
                    names
                        .buffers
                        .iter()
                        .find(|(id, _)| Some(*id) == n)
                        .map(|(_, name)| name.clone())
                        .ok_or(ExpandError::NoAlternateFile)?
                }
            }
            c => {
                out.push(c);
                continue;
            }
        };
        let (name, after) = apply_filename_modifiers(name, rest);
        out.push_str(&name);
        rest = after;
    }
    Ok(out)
}

/// Apply the file-name modifiers at the start of `mods` to `name`; returns the result and the text after
/// the modifiers.
fn apply_filename_modifiers(mut name: String, mut mods: &str) -> (String, &str) {
    // `:e` after `:e` widens the extension by one (`k.tar.gz` -> `gz` -> `tar.gz`): the name before the
    // first `:e`, and how many extensions to keep.
    let mut ext_of: Option<(String, usize)> = None;
    while let Some(m) = mods.strip_prefix(':').and_then(|r| r.chars().next()) {
        if !"p~.htre".contains(m) {
            break;
        }
        mods = &mods[2..];
        if m != 'e' {
            ext_of = None;
        }
        name = match m {
            'p' => full_path(&name),
            '~' => home_relative(&full_path(&name)).unwrap_or(name),
            '.' => cwd_relative(&full_path(&name)).unwrap_or(name),
            'h' => head(&name),
            't' => name.rsplit('/').next().unwrap_or("").to_string(),
            'r' => {
                let (root, _) = split_extension(&name);
                root.to_string()
            }
            _ => {
                let (base, n) = match ext_of.take() {
                    Some((base, n)) => (base, n + 1),
                    None => (name.clone(), 1),
                };
                let mut root = base.as_str();
                for _ in 0..n {
                    root = split_extension(root).0;
                }
                let ext = base
                    .get(root.len()..)
                    .and_then(|e| e.strip_prefix('.'))
                    .unwrap_or("")
                    .to_string();
                ext_of = Some((base, n));
                ext
            }
        };
    }
    (name, mods)
}

/// `:p`: the absolute path (a directory gets a trailing `/`, as in Vim).
fn full_path(name: &str) -> String {
    let path = std::path::Path::new(name);
    let abs =
        std::path::absolute(path).map_or_else(|_| name.to_string(), |p| p.display().to_string());
    let abs = abs.replace("/./", "/");
    if path.is_dir() && !abs.ends_with('/') {
        format!("{abs}/")
    } else {
        abs
    }
}

/// `:~`: `abs` with the home directory written `~`, when it is under it.
fn home_relative(abs: &str) -> Option<String> {
    let home = std::env::var("HOME").ok().filter(|h| !h.is_empty())?;
    let rest = abs.strip_prefix(home.trim_end_matches('/'))?;
    (rest.is_empty() || rest.starts_with('/')).then(|| format!("~{rest}"))
}

/// `:.`: `abs` relative to the working directory, when it is under it.
fn cwd_relative(abs: &str) -> Option<String> {
    let cwd = std::env::current_dir().ok()?;
    let cwd = cwd.to_str()?.trim_end_matches('/');
    let rest = abs.strip_prefix(cwd)?.strip_prefix('/')?;
    Some(if rest.is_empty() {
        ".".into()
    } else {
        rest.into()
    })
}

/// `:h`: the name without its last component (`.` when it has no directory part); a trailing `/` alone
/// is removed first.
fn head(name: &str) -> String {
    if let Some(dir) = name.strip_suffix('/').filter(|d| !d.is_empty()) {
        return dir.to_string();
    }
    match name.rfind('/') {
        Some(0) => "/".into(),
        Some(i) => name[..i].to_string(),
        None => ".".into(),
    }
}

/// Split the last extension off `name`: `("dir/k.tar", "gz")` for `dir/k.tar.gz`. A dot that starts the
/// tail (`.bashrc`) is not an extension.
fn split_extension(name: &str) -> (&str, &str) {
    let tail_at = name.rfind('/').map_or(0, |i| i + 1);
    match name[tail_at..].rfind('.') {
        Some(i) if i > 0 => (&name[..tail_at + i], &name[tail_at + i + 1..]),
        _ => (name, ""),
    }
}
